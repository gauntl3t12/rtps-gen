use clap::{arg, command, value_parser, ArgAction};
use omg_idl_code_gen::{generate_with_search_path, Configuration};
use std::{
    fs::File,
    io::{stdout, Error, ErrorKind},
    path::PathBuf,
};

fn main() -> Result<(), std::io::Error> {
    let matches = command!()
    .arg(
        arg!(
            -I --include_dir <DIR> "Add the specified 'directory' to the search path for include files."
        )
        .default_value(".")
        .required(false)
        .value_parser(value_parser!(PathBuf)),
    )
    .arg(
        arg!(
            -v --verbose ... "Turn verbose logging on"
        )
        .required(false)
        .action(ArgAction::SetTrue)
    )
    .arg(
        arg!(
            -o --output_file <FILE> "Write output to 'outfile'."
        )
        .required(false)
        .value_parser(value_parser!(PathBuf)),
    )
    .arg(
        arg!(
            [idl_file] "IDL File to parse"
        )
        .required(true)
        .value_parser(value_parser!(PathBuf)),)
    .get_matches();

    let search_path = matches
        .get_one::<PathBuf>("include_dir")
        .expect("include_dir is defaulted");

    let idl_file = matches
        .get_one::<PathBuf>("idl_file")
        .expect("idl_file is required");

    let config = Configuration::new(search_path, idl_file, matches.get_flag("verbose"));

    let result = match matches.get_one::<PathBuf>("output_file") {
        Some(outfile) => {
            let mut of = File::create(outfile)?;
            generate_with_search_path(&mut of, &config)
        }
        _ => generate_with_search_path(&mut stdout(), &config),
    };

    match result {
        Ok(_) => Ok(()),
        Err(err) => {
            eprint!("parse error {:?}", err);
            Err(Error::new(ErrorKind::InvalidData, "parse error"))
        }
    }
}

#[cfg(test)]
mod tests {
    use omg_idl_code_gen::{generate_with_search_path, Configuration};
    use std::{
        fs,
        fs::File,
        io::{Read, Seek, SeekFrom, Write},
        path::Path,
        str,
    };
    use tempfile::{Builder, NamedTempFile};
    use trybuild;

    #[test]
    fn expected_mappings() {
        let test_dirs = [
            "files/test-vectors/const_str/",
            "files/test-vectors/double_module_depth/",
            "files/test-vectors/module_use_diff_module/",
            "files/test-vectors/typedef_long/",
            "files/test-vectors/typedef_long_long/",
            "files/test-vectors/typedef_short/",
            "files/test-vectors/typedef_octet/",
            "files/test-vectors/typedef_unsigned_short/",
            "files/test-vectors/typedef_unsigned_long",
            "files/test-vectors/typedef_unsigned_long_long",
            "files/test-vectors/typedef_char",
            "files/test-vectors/typedef_wchar",
            "files/test-vectors/typedef_string",
            "files/test-vectors/typedef_wstring",
            "files/test-vectors/typedef_string_bounded",
            "files/test-vectors/typedef_wstring_bounded",
            "files/test-vectors/typedef_sequence",
            "files/test-vectors/typedef_array_dim_1",
            "files/test-vectors/typedef_array_dim_2",
            "files/test-vectors/struct_members",
            "files/test-vectors/enum_variants",
            "files/test-vectors/struct_module",
            "files/test-vectors/const_op_and",
            "files/test-vectors/const_op_add",
            "files/test-vectors/const_op_sub",
            "files/test-vectors/const_op_lshift",
            "files/test-vectors/const_op_rshift",
            "files/test-vectors/const_op_or",
            "files/test-vectors/const_op_xor",
            "files/test-vectors/const_op_mul",
            "files/test-vectors/const_op_div",
            "files/test-vectors/const_op_mod",
            "files/test-vectors/include_directive/",
            "files/test-vectors/union_members",
        ];

        // TestCases must go out of scope before tmp_file goes out of scope
        // to ensure the test is executed prior to the file(s) being deleted.
        let mut test_files: Vec<NamedTempFile> = Vec::new();
        {
            let t = trybuild::TestCases::new();
            for test_dir in test_dirs {
                println!("Testing directory: {test_dir}");
                let mut tmp_file = Builder::new().suffix(".rs").tempfile().unwrap();
                testvector_verify(test_dir, tmp_file.as_file_mut());
                t.pass(tmp_file.path());
                test_files.push(tmp_file);
            }
        }
    }

    #[test]
    fn self_include_does_not_recurse_forever() {
        let temp_dir = tempfile::tempdir().unwrap();
        let input = r#"#include "./input.idl"
module DDS {
    typedef long Foo;
};"#;
        fs::write(temp_dir.path().join("input.idl"), input).unwrap();

        let config = Configuration::new(temp_dir.path(), Path::new("input.idl"), false);
        let mut out = tempfile::tempfile().unwrap();
        let result = generate_with_search_path(&mut out, &config);

        assert!(
            result.is_ok(),
            "self include should be handled without recursion"
        );
        out.seek(SeekFrom::Start(0)).unwrap();
        let mut generated = String::new();
        out.read_to_string(&mut generated).unwrap();
        assert!(
            generated.contains("pub type Foo = i32;"),
            "missing expected typedef in output"
        );
    }

    fn testvector_verify(testvector: &str, tmp_file: &mut File) {
        let expected = {
            let expected_path = Path::new(testvector).join("expected.rs");
            let mut expected_file = match File::open(expected_path) {
                Ok(file) => file,
                Err(err) => {
                    eprintln!("{}", err);
                    panic!();
                }
            };
            let mut expected = String::new();
            assert!(expected_file.read_to_string(&mut expected).is_ok());
            expected
        };

        let generated = {
            let config = Configuration::new(Path::new(testvector), Path::new("input.idl"), false);
            match generate_with_search_path(tmp_file, &config) {
                Ok(_) => (),
                Err(err) => {
                    eprint!("parse error {:?}", err);
                    panic!();
                }
            };

            let _ = tmp_file.seek(SeekFrom::Start(0));
            let mut generated = String::new();
            assert!(tmp_file.read_to_string(&mut generated).is_ok());

            let main_str = "fn main() {}";
            let _ = write!(tmp_file, "{main_str}");

            let _ = tmp_file.seek(SeekFrom::Start(0));
            generated
        };

        println!("-------------\n{generated}");

        let expected_no_carriage: Vec<u8> = expected
            .as_bytes()
            .iter()
            .filter(|&&b| b != b'\r')
            .copied()
            .collect();
        let text_no_carriage: Vec<u8> = generated
            .as_bytes()
            .iter()
            .filter(|&&b| b != b'\r')
            .copied()
            .collect();
        assert_eq!(expected_no_carriage.as_slice(), text_no_carriage.as_slice());
    }
}
