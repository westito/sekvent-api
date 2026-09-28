//! Compiling a `[contract]` root in-process with protox: no `protoc`, no
//! cargo build.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow, bail};
use prost_types::field_descriptor_proto::Type;
use prost_types::{DescriptorProto, FileDescriptorProto};

use super::model::qualify;

/// Name prefix of the well-known type files protox bundles.
const WELL_KNOWN_FILES: &str = "google/protobuf/";

/// The compiled files of one root.
#[derive(Debug)]
pub(crate) struct CompiledRoot {
    /// Every file, imports included, dependencies first.
    pub(crate) files: Vec<FileDescriptorProto>,
    /// Names of the files that lie in the root itself; only their services
    /// are contracts.
    pub(crate) own: BTreeSet<String>,
}

/// Compile every `.proto` below `project_root/root`, sorted, with the
/// include path `[root] + includes`.
pub(crate) fn compile_root(
    project_root: &Path,
    root: &Path,
    includes: &[PathBuf],
) -> anyhow::Result<CompiledRoot> {
    let dir = project_root.join(root);
    if !dir.is_dir() {
        bail!("contract root `{}` is not a directory", root.display());
    }
    let mut include_path = vec![dir.clone()];
    for include in includes {
        let path = project_root.join(include);
        if !path.is_dir() {
            bail!(
                "contract include `{}` is not a directory",
                include.display()
            );
        }
        include_path.push(path);
    }
    let sources = proto_files(&dir)?;
    if sources.is_empty() {
        bail!("contract root `{}` holds no .proto files", root.display());
    }
    let mut compiler = protox::Compiler::new(include_path).map_err(|error| protox_error(&error))?;
    compiler.include_imports(true).include_source_info(false);
    compiler
        .open_files(sources)
        .map_err(|error| protox_error(&error))?;
    let own = compiler
        .files()
        .filter(|file| !file.is_import())
        .map(|file| file.name().to_owned())
        .collect();
    let files = compiler.file_descriptor_set().file;
    for file in &files {
        validate_file(file)?;
    }
    Ok(CompiledRoot { files, own })
}

/// protox's `Debug` form names the file, line and column; `Display` does not.
fn protox_error(error: &protox::Error) -> anyhow::Error {
    anyhow!("{error:?}")
}

/// Every `.proto` file below `dir`, sorted by path.
pub(crate) fn proto_files(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        let entries = std::fs::read_dir(&current)
            .with_context(|| format!("cannot read {}", current.display()))?;
        for entry in entries {
            let path = entry
                .with_context(|| format!("cannot read {}", current.display()))?
                .path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "proto") {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Reject what the canonical form cannot express: editions files and
/// group fields. The bundled well-known type files are trusted.
pub(crate) fn validate_file(file: &FileDescriptorProto) -> anyhow::Result<()> {
    if file.name().starts_with(WELL_KNOWN_FILES) {
        return Ok(());
    }
    if file.syntax() == "editions" {
        bail!(
            "{}: proto editions are not supported by `cargo sekvent contract`",
            file.name()
        );
    }
    file.message_type
        .iter()
        .try_for_each(|message| reject_groups(file.name(), file.package(), message))
}

fn reject_groups(file: &str, prefix: &str, message: &DescriptorProto) -> anyhow::Result<()> {
    let name = qualify(prefix, message.name());
    if let Some(field) = message
        .field
        .iter()
        .find(|field| field.r#type() == Type::Group)
    {
        bail!(
            "{file}: field `{}` of `{name}` is a group, which `cargo sekvent contract` does not support",
            field.name()
        );
    }
    message
        .nested_type
        .iter()
        .try_for_each(|nested| reject_groups(file, &name, nested))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::testing::{load_project, write_files};

    #[test]
    fn proto_files_are_found_recursively_and_sorted() {
        let dir = tempfile::tempdir().unwrap();
        write_files(
            dir.path(),
            &[
                ("b.proto", ""),
                ("a/c.proto", ""),
                ("a.proto", ""),
                ("a/notes.txt", ""),
                ("z/y/x.proto", ""),
            ],
        );
        let found: Vec<PathBuf> = proto_files(dir.path())
            .unwrap()
            .into_iter()
            .map(|path| path.strip_prefix(dir.path()).unwrap().to_owned())
            .collect();
        assert_eq!(
            found,
            [
                PathBuf::from("a/c.proto"),
                PathBuf::from("a.proto"),
                PathBuf::from("b.proto"),
                PathBuf::from("z/y/x.proto"),
            ]
        );
        assert!(proto_files(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn includes_resolve_imports_but_contribute_no_services() {
        let dir = tempfile::tempdir().unwrap();
        write_files(
            dir.path(),
            &[
                (
                    "vendor/ext/v1/ext.proto",
                    "syntax = \"proto3\";\npackage ext.v1;\nmessage Ref { string id = 1; }\n\
                     service Foreign { rpc Call(Ref) returns (Ref); }\n",
                ),
                (
                    "proto/own/v1/own.proto",
                    "syntax = \"proto3\";\npackage own.v1;\nimport \"ext/v1/ext.proto\";\n\
                     service Own { rpc Get(ext.v1.Ref) returns (ext.v1.Ref); }\n",
                ),
            ],
        );
        let compiled =
            compile_root(dir.path(), Path::new("proto"), &[PathBuf::from("vendor")]).unwrap();
        assert_eq!(
            compiled.own.iter().collect::<Vec<_>>(),
            ["own/v1/own.proto"]
        );
        let names: Vec<&str> = compiled
            .files
            .iter()
            .map(FileDescriptorProto::name)
            .collect();
        assert_eq!(names, ["ext/v1/ext.proto", "own/v1/own.proto"]);

        let contracts = load_project(dir.path(), &["proto"], &["vendor"]).unwrap();
        assert_eq!(contracts.keys().collect::<Vec<_>>(), ["own.v1.Own"]);
        assert!(contracts["own.v1.Own"].messages.contains_key("ext.v1.Ref"));

        let error = load_project(dir.path(), &["proto"], &[]).unwrap_err();
        assert!(error.to_string().contains("ext/v1/ext.proto"), "{error:#}");
    }

    #[test]
    fn missing_or_empty_directories_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        write_files(dir.path(), &[("empty/readme.txt", "")]);
        let error = compile_root(dir.path(), Path::new("nope"), &[]).unwrap_err();
        assert_eq!(error.to_string(), "contract root `nope` is not a directory");
        let error =
            compile_root(dir.path(), Path::new("empty"), &[PathBuf::from("gone")]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "contract include `gone` is not a directory"
        );
        let error = compile_root(dir.path(), Path::new("empty"), &[]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "contract root `empty` holds no .proto files"
        );
    }

    #[test]
    fn a_syntax_error_names_the_file_and_line() {
        let dir = tempfile::tempdir().unwrap();
        write_files(
            dir.path(),
            &[("proto/bad.proto", "syntax = \"proto3\";\nmessage {\n")],
        );
        let error = compile_root(dir.path(), Path::new("proto"), &[]).unwrap_err();
        assert!(error.to_string().starts_with("bad.proto:2:"), "{error}");
    }

    #[test]
    fn editions_files_are_rejected_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        write_files(
            dir.path(),
            &[(
                "proto/new.proto",
                "edition = \"2023\";\npackage n;\nmessage M { string a = 1; }\n",
            )],
        );
        let error = compile_root(dir.path(), Path::new("proto"), &[]).unwrap_err();
        assert!(error.to_string().contains("new.proto"), "{error}");

        let file = FileDescriptorProto {
            name: Some("new.proto".into()),
            syntax: Some("editions".into()),
            ..FileDescriptorProto::default()
        };
        assert_eq!(
            validate_file(&file).unwrap_err().to_string(),
            "new.proto: proto editions are not supported by `cargo sekvent contract`"
        );
        let bundled = FileDescriptorProto {
            name: Some("google/protobuf/x.proto".into()),
            syntax: Some("editions".into()),
            ..FileDescriptorProto::default()
        };
        assert!(validate_file(&bundled).is_ok());
    }

    #[test]
    fn group_fields_are_rejected_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        write_files(
            dir.path(),
            &[(
                "proto/old.proto",
                "syntax = \"proto2\";\npackage old;\nmessage Outer {\n  message Inner {\n    \
                 optional group Data = 1 { optional int32 x = 2; }\n  }\n  \
                 optional Inner inner = 1;\n}\n",
            )],
        );
        let error = compile_root(dir.path(), Path::new("proto"), &[]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "old.proto: field `data` of `old.Outer.Inner` is a group, which `cargo sekvent \
             contract` does not support"
        );
    }
}
