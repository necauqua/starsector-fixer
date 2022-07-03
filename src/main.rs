use std::{
    borrow::Cow,
    fs::File,
    io::{Cursor, Read, Seek, SeekFrom, Write},
    path::PathBuf,
};

use anyhow::{bail, ensure, Context, Result};
use byteorder::{ReadBytesExt, BE};
use log::LevelFilter;
use structopt::StructOpt;

use zip::{write::FileOptions, ZipArchive, ZipWriter};

/// A simple program that remaps Java method names to not have dots in them.
///
/// Old Oracle VMs allow that, which is against the spec, and some obfuscation
/// methods (cough-cough, starsector) use that, making the resulting program
/// not runnable on VMs with a stricter implementation, such as OpenJDK
#[derive(Debug, StructOpt)]
struct Opt {
    /// The path to the JAR file to be processed
    input: PathBuf,
    /// The output file. Without this option, a backup is created for the input
    /// file and the input file gets replaced with the fixed one
    #[structopt(short, long)]
    output: Option<PathBuf>,
    /// Use this flag if you don't want the backup to be created. Does
    /// nothing if -o is present
    #[structopt(short, long)]
    force: bool,
}

fn main() -> Result<()> {
    env_logger::builder()
        .format_timestamp(None)
        .format_target(false)
        .filter_level(LevelFilter::Info)
        .parse_env(env_logger::Env::default())
        .init();

    let opt = Opt::from_args();

    let in_place = opt.output.is_none();
    let work_file = opt
        .output
        .unwrap_or_else(|| opt.input.with_extension("jar.temp"));

    let input = File::open(&opt.input)
        .with_context(|| format!("Reading archive {}", opt.input.display()))?;
    let mut output = ZipWriter::new(File::create(&work_file)?);
    let mut zip = ZipArchive::new(input)?;

    for i in 0..zip.len() {
        let mut file = zip.by_index(i)?;
        if !file.is_file() || !file.name().ends_with(".class") {
            drop(file); // release the `&mut zip` used by `file`
            output.raw_copy_file(zip.by_index_raw(i)?)?;
            continue;
        }
        let mut buf = Vec::with_capacity(8096);
        file.read_to_end(&mut buf)?;

        log::debug!("Checking {}", file.name());
        if let Some(updated_bytecode) =
            fix_class(&buf, file.name()).with_context(|| format!("Processing {}", file.name()))?
        {
            log::info!("Processed {}", file.name());
            let mut options = FileOptions::default()
                .large_file(file.compressed_size().max(file.size()) > u32::MAX as u64)
                .last_modified_time(file.last_modified())
                .compression_method(file.compression());
            if let Some(perms) = file.unix_mode() {
                options = options.unix_permissions(perms);
            }
            output.start_file(file.name(), options)?;
            output.write_all(&updated_bytecode)?;
        } else {
            drop(file); // ditto
            output.raw_copy_file(zip.by_index_raw(i)?)?
        }
    }

    if in_place {
        if !opt.force {
            std::fs::copy(&opt.input, opt.input.with_extension("jar.bak"))
                .context("Creating backup")?;
        }
        std::fs::rename(work_file, opt.input)
            .context("Moving the file that was worked on in place of the original")?;
    }

    Ok(())
}

fn fix_class(bytecode: &[u8], filename: &str) -> Result<Option<Vec<u8>>> {
    let mut stream = Cursor::new(bytecode);

    ensure!(stream.read_u32::<BE>()? == 0xCAFEBABE, "Bad magic number");

    // skip u2 minor_version, u2 major_version
    stream.seek(SeekFrom::Current(4))?;

    let constant_pool_count = stream.read_u16::<BE>()? as usize;
    log::debug!("Constant pool entry count is {}", constant_pool_count);

    let mut constant_pool = Vec::with_capacity(constant_pool_count);
    // take up unused zeroeth index
    constant_pool.push(ConstantItem::Ignored);

    while constant_pool.len() < constant_pool_count {
        let constant_item = ConstantItem::read(&mut stream)?;
        log::debug!("Constant #{} is {:?}", constant_pool.len(), constant_item);
        if matches!(constant_item, ConstantItem::DoubleEntry) {
            // longs and doubles take up two indices
            constant_pool.push(ConstantItem::Ignored);
        }
        constant_pool.push(constant_item);
    }

    // u2 access_flags, u2 this_class, u2 super_class
    stream.seek(SeekFrom::Current(6))?;

    let interfaces_count = stream.read_u16::<BE>()?;

    // skip u2 interfaces[interfaces_count]
    stream.seek(SeekFrom::Current(2 * interfaces_count as i64))?;

    let mut member_name_indices = Vec::new();

    let mut read_member_name_indices = |member_type: &str| -> Result<()> {
        let count = stream.read_u16::<BE>()?;
        log::debug!("{} count is {}", member_type, count);
        for _ in 0..count {
            stream.seek(SeekFrom::Current(2))?; // u2 access_flags;
            let name_index = stream.read_u16::<BE>()?;
            log::debug!(
                "{} name is a constant #{} -> {:?}",
                member_type,
                name_index,
                constant_pool[name_index as usize],
            );
            member_name_indices.push(name_index);
            stream.seek(SeekFrom::Current(2))?; // u2 descriptor_index;

            // yeah all of the below is just a complicated skip
            let attributes_count = stream.read_u16::<BE>()?;
            for _ in 0..attributes_count {
                stream.seek(SeekFrom::Current(2))?; // u2 attribute_name_index;
                let attribute_length = stream.read_u32::<BE>()?;
                stream.seek(SeekFrom::Current(attribute_length as i64))?; // u1 info[attribute_length];
            }
        }
        Ok(())
    };

    read_member_name_indices("Field")?;
    read_member_name_indices("Method")?;

    let mut updated = None;

    let mut fix_name = |idx: usize| -> Result<()> {
        if let ConstantItem::Utf8(s, class_offset) = &constant_pool[idx] {
            if let Some(idx) = s.find('.') {
                let owned = updated.get_or_insert_with(|| bytecode.to_owned());
                let char = &mut owned[class_offset + idx];
                // could've already been fixed by field/method def or other ref
                if *char == '.' as u8 {
                    log::info!("Fixing bad name '{}' in {}", s, filename);
                    *char = '_' as u8;
                }
            }
            Ok(())
        } else {
            bail!("Constant #{} is not a UTF8_INFO", idx);
        }
    };

    for member_name_idx in member_name_indices {
        fix_name(member_name_idx as usize)?;
    }

    for constant in &constant_pool {
        if let ConstantItem::Ref(ref_idx) = constant {
            if let ConstantItem::NameAndType(name_idx) = constant_pool[*ref_idx as usize] {
                fix_name(name_idx as usize)?;
            }
        }
    }

    Ok(updated)
}

const UTF_8: u8 = 1;
const INTEGER: u8 = 3;
const FLOAT: u8 = 4;
const LONG: u8 = 5;
const DOUBLE: u8 = 6;
const CLASS: u8 = 7;
const STRING: u8 = 8;
const FIELD_REF: u8 = 9;
const METHOD_REF: u8 = 10;
const INTERFACE_METHOD_REF: u8 = 11;
const NAME_AND_TYPE: u8 = 12;
const METHOD_HANDLE: u8 = 15;
const METHOD_TYPE: u8 = 16;
const INVOKE_DYNAMIC: u8 = 18;
const MODULE: u8 = 19;
const PACKAGE: u8 = 20;

#[derive(Debug)]
enum ConstantItem<'a> {
    Utf8(Cow<'a, str>, usize),
    Ref(u16),
    NameAndType(u16),
    DoubleEntry,
    Ignored,
}

impl<'a> ConstantItem<'a> {
    fn read(stream: &mut Cursor<&'a [u8]>) -> Result<Self> {
        Ok(match stream.read_u8()? {
            UTF_8 => {
                let len = stream.read_u16::<BE>()? as usize;
                let pos = stream.position() as usize;
                let slice = &stream.get_ref()[pos..pos + len];
                stream.seek(SeekFrom::Current(len as i64))?;
                Self::Utf8(cesu8::from_java_cesu8(slice)?, pos)
            }
            FIELD_REF | METHOD_REF | INTERFACE_METHOD_REF => {
                stream.seek(SeekFrom::Current(2))?; // skip u2 class_index
                let name_and_type_index = stream.read_u16::<BE>()?;
                Self::Ref(name_and_type_index)
            }
            NAME_AND_TYPE => {
                let name_index = stream.read_u16::<BE>()?;
                stream.seek(SeekFrom::Current(2))?; // skip u2 descriptor_index
                Self::NameAndType(name_index)
            }
            CLASS | STRING | METHOD_TYPE | MODULE | PACKAGE => {
                stream.seek(SeekFrom::Current(2))?;
                Self::Ignored
            }
            METHOD_HANDLE => {
                stream.seek(SeekFrom::Current(3))?;
                Self::Ignored
            }
            INTEGER | FLOAT | INVOKE_DYNAMIC => {
                stream.seek(SeekFrom::Current(4))?;
                Self::Ignored
            }
            LONG | DOUBLE => {
                stream.seek(SeekFrom::Current(8))?;
                Self::DoubleEntry
            }
            x => bail!("Unknown constant tag: {}", x),
        })
    }
}
