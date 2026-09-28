//! Compiling a `[contract]` root in-process with protox: no `protoc`, no
//! cargo build.
//!
//! Every directory and every file read lies inside the project: roots and
//! includes are canonicalized and confined to it, symlinked directories are
//! not walked, and a file whose real path leaves the project is refused
//! before it is read.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow, bail};
use prost_types::field_descriptor_proto::Type;
use prost_types::{DescriptorProto, FileDescriptorProto};
use protox::file::{
    ChainFileResolver, File, FileResolver, GoogleFileResolver, IncludeFileResolver,
};

use super::model::qualify;

/// The compiled files of one root.
#[derive(Debug)]
pub(crate) struct CompiledRoot {
    /// Every file, imports included, dependencies first.
    pub(crate) files: Vec<FileDescriptorProto>,
    /// Names of the files that lie in the root itself; only their services
    /// are contracts.
    pub(crate) own: BTreeSet<String>,
    /// Names of the well-known type files protox bundled; a user file is
    /// never among them, whatever its package.
    pub(crate) bundled: BTreeSet<String>,
}

/// Compile every `.proto` below `project_root/root`, sorted, with the
/// include path `[root] + includes`. Root and includes must resolve to
/// directories inside `project_root`.
pub(crate) fn compile_root(
    project_root: &Path,
    root: &Path,
    includes: &[PathBuf],
) -> anyhow::Result<CompiledRoot> {
    let project = project_root
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", project_root.display()))?;
    let dir = confined_dir(&project, "contract.roots", "contract root", root)?;
    let mut include_path = vec![dir.clone()];
    for include in includes {
        include_path.push(confined_dir(
            &project,
            "contract.includes",
            "contract include",
            include,
        )?);
    }
    let sources = proto_files(&dir)?;
    if sources.is_empty() {
        bail!("contract root `{}` holds no .proto files", root.display());
    }
    let mut resolver = ChainFileResolver::new();
    for include in include_path {
        resolver.add(ConfinedResolver {
            project: project.clone(),
            include: include.clone(),
            inner: IncludeFileResolver::new(include),
        });
    }
    resolver.add(GoogleFileResolver::new());
    let mut compiler = protox::Compiler::with_file_resolver(resolver);
    compiler.include_imports(true).include_source_info(false);
    compiler
        .open_files(sources)
        .map_err(|error| protox_error(&error))?;
    let own = compiler
        .files()
        .filter(|file| !file.is_import())
        .map(|file| file.name().to_owned())
        .collect();
    let bundled: BTreeSet<String> = compiler
        .files()
        .filter(|file| file.path().is_none())
        .map(|file| file.name().to_owned())
        .collect();
    let files = compiler.file_descriptor_set().file;
    for file in files.iter().filter(|file| !bundled.contains(file.name())) {
        validate_file(file)?;
    }
    Ok(CompiledRoot {
        files,
        own,
        bundled,
    })
}

/// `project.join(dir)` canonicalized; it must be a directory inside
/// `project`. `key` names the configuration entry, `what` the directory.
fn confined_dir(project: &Path, key: &str, what: &str, dir: &Path) -> anyhow::Result<PathBuf> {
    let Some(real) = project
        .join(dir)
        .canonicalize()
        .ok()
        .filter(|real| real.is_dir())
    else {
        bail!("{what} `{}` is not a directory", dir.display());
    };
    if !real.starts_with(project) {
        bail!(
            "{key}: `{}` lies outside the project directory",
            dir.display()
        );
    }
    Ok(real)
}

/// An include directory that refuses files whose real path leaves the
/// project, before reading them.
struct ConfinedResolver {
    project: PathBuf,
    include: PathBuf,
    inner: IncludeFileResolver,
}

impl FileResolver for ConfinedResolver {
    fn resolve_path(&self, path: &Path) -> Option<String> {
        self.inner.resolve_path(path)
    }

    fn open_file(&self, name: &str) -> Result<File, protox::Error> {
        if let Ok(real) = self.include.join(name).canonicalize()
            && !real.starts_with(&self.project)
        {
            return Err(protox::Error::new(OutsideProject(name.to_owned())));
        }
        self.inner.open_file(name)
    }
}

/// A proto file whose real path lies outside the project.
struct OutsideProject(String);

impl fmt::Display for OutsideProject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: the file resolves outside the project directory",
            self.0
        )
    }
}

impl fmt::Debug for OutsideProject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for OutsideProject {}

/// protox's `Debug` form names the file, line and column; `Display` does not.
fn protox_error(error: &protox::Error) -> anyhow::Error {
    anyhow!("{error:?}")
}

/// Every `.proto` file below `dir`, sorted by path. Symlinked directories
/// are not walked, so a link cycle cannot loop.
pub(crate) fn proto_files(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        let entries = std::fs::read_dir(&current)
            .with_context(|| format!("cannot read {}", current.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("cannot read {}", current.display()))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .with_context(|| format!("cannot read {}", path.display()))?;
            if file_type.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "proto") && path.is_file() {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Reject what the canonical form cannot express: editions files and
/// group fields. Only user files are validated; the files protox bundles
/// are skipped by the caller.
pub(crate) fn validate_file(file: &FileDescriptorProto) -> anyhow::Result<()> {
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
        let lookalike = FileDescriptorProto {
            name: Some("google/protobuf/x.proto".into()),
            syntax: Some("editions".into()),
            ..FileDescriptorProto::default()
        };
        assert!(
            validate_file(&lookalike).is_err(),
            "a file is exempt only when protox bundled it, not by its name"
        );
    }

    #[test]
    fn only_the_files_protox_bundles_count_as_well_known() {
        let dir = tempfile::tempdir().unwrap();
        write_files(
            dir.path(),
            &[
                (
                    "proto/google/protobuf/mine.proto",
                    "syntax = \"proto3\";\npackage google.protobuf;\n\
                     message Mine { string a = 1; }\n",
                ),
                (
                    "proto/svc.proto",
                    "syntax = \"proto3\";\npackage svc.v1;\n\
                     import \"google/protobuf/mine.proto\";\n\
                     import \"google/protobuf/empty.proto\";\n\
                     service Svc { rpc Get(google.protobuf.Mine) returns \
                     (google.protobuf.Empty); }\n",
                ),
            ],
        );
        let compiled = compile_root(dir.path(), Path::new("proto"), &[]).unwrap();
        assert_eq!(
            compiled.bundled.iter().collect::<Vec<_>>(),
            ["google/protobuf/empty.proto"]
        );
        assert!(compiled.own.contains("google/protobuf/mine.proto"));

        let contracts = load_project(dir.path(), &["proto"], &[]).unwrap();
        let svc = &contracts["svc.v1.Svc"];
        assert_eq!(
            svc.messages.keys().collect::<Vec<_>>(),
            ["google.protobuf.Mine"],
            "a user file in package google.protobuf is expanded, the bundled Empty is not"
        );

        write_files(
            dir.path(),
            &[(
                "proto/google/protobuf/mine.proto",
                "syntax = \"proto2\";\npackage google.protobuf;\n\
                 message Mine { optional group G = 1 { optional int32 x = 2; } }\n",
            )],
        );
        let error = compile_root(dir.path(), Path::new("proto"), &[]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "google/protobuf/mine.proto: field `g` of `google.protobuf.Mine` is a group, which \
             `cargo sekvent contract` does not support"
        );
    }

    #[test]
    fn roots_and_includes_outside_the_project_are_refused() {
        let outer = tempfile::tempdir().unwrap();
        write_files(
            outer.path(),
            &[
                ("elsewhere/x.proto", "syntax = \"proto3\";\n"),
                ("project/proto/a.proto", "syntax = \"proto3\";\n"),
            ],
        );
        let project = outer.path().join("project");
        let error = compile_root(&project, Path::new("../elsewhere"), &[]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "contract.roots: `../elsewhere` lies outside the project directory"
        );
        let absolute = outer.path().join("elsewhere");
        let error = compile_root(&project, &absolute, &[]).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "contract.roots: `{}` lies outside the project directory",
                absolute.display()
            )
        );
        let error = compile_root(
            &project,
            Path::new("proto"),
            &[PathBuf::from("../elsewhere")],
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "contract.includes: `../elsewhere` lies outside the project directory"
        );
        assert!(compile_root(&project, Path::new("proto/../proto"), &[]).is_ok());
        let error = compile_root(&outer.path().join("gone"), Path::new("proto"), &[]).unwrap_err();
        assert!(error.to_string().starts_with("cannot resolve "), "{error}");
        assert_eq!(
            format!("{:?}", OutsideProject("a.proto".into())),
            "a.proto: the file resolves outside the project directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_never_lead_outside_the_project_or_into_a_loop() {
        use std::os::unix::fs::symlink;

        let outer = tempfile::tempdir().unwrap();
        write_files(
            outer.path(),
            &[
                (
                    "secret/leak.proto",
                    "syntax = \"proto3\";\npackage leak;\nmessage L {}\n",
                ),
                (
                    "project/proto/svc.proto",
                    "syntax = \"proto3\";\npackage svc.v1;\nmessage M {}\n\
                     service Svc { rpc Get(M) returns (M); }\n",
                ),
            ],
        );
        let project = outer.path().join("project");
        let proto = project.join("proto");
        symlink(&proto, proto.join("loop")).unwrap();
        symlink(&project, proto.join("up")).unwrap();
        let found = proto_files(&proto).unwrap();
        assert_eq!(
            found,
            [proto.join("svc.proto")],
            "linked directories are not walked"
        );
        assert!(compile_root(&project, Path::new("proto"), &[]).is_ok());

        symlink(outer.path().join("secret"), project.join("linked")).unwrap();
        let error = compile_root(&project, Path::new("linked"), &[]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "contract.roots: `linked` lies outside the project directory"
        );

        symlink(
            outer.path().join("secret/leak.proto"),
            proto.join("leak.proto"),
        )
        .unwrap();
        let error = compile_root(&project, Path::new("proto"), &[]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "leak.proto: the file resolves outside the project directory"
        );
        std::fs::remove_file(proto.join("leak.proto")).unwrap();

        write_files(
            &proto,
            &[(
                "user.proto",
                "syntax = \"proto3\";\npackage user;\nimport \"ext/leak.proto\";\n",
            )],
        );
        symlink(outer.path().join("secret"), proto.join("ext")).unwrap();
        let error = compile_root(&project, Path::new("proto"), &[]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("ext/leak.proto: the file resolves outside the project directory"),
            "{error}"
        );
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
