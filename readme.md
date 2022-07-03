# starsector-fixer
This is a simple program that reads the class files in the provided jar file,
and fixes field and method names to not contain dots in them (by replacing them
with underscores).

This needs to be done, because having dots in member names is actually
prohibited by the JVM spec, however, Oracle JDK 7 allowed that, which lead to
developers of Starsector to be able to use some sort of obfuscation that has
such dots.

This means that you cannot use, say, OpenJDK 8 to run the game, since it would
just immediately crash with `ClassFormatError`.

See https://fractalsoftworks.com/forum/index.php?topic=18561.0

## Compiling
It's just a small Rust program, `cargo build --release` should be enough.

## Running
`starsector-fixer -h` :) 

## License
Just MIT, if you want to share the program in any form, don't forget to keep
the LICENSE file (in any form), it has my name on top of it :)
