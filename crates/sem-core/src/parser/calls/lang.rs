//! The seam a language plugs into. A language supplies three things:
//! a lowering (syntax tree -> [`FileFacts`]), a layout (how files form a
//! module tree and which crates/packages are importable by name), and a few
//! tables of *data* about its standard library. The name-resolution, type
//! inference and target-selection stages are shared and never branch on the
//! language.

use std::path::Path as FsPath;

use rustc_hash::FxHashMap as HashMap;

use super::ir::FileFacts;

/// How a standard-library method (one not defined in the repo) transforms
/// the type of its receiver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinRet {
    /// Returns the receiver's type unchanged (`clone`, `as_ref` on Option).
    Same,
    /// Returns the receiver's n-th type argument (`Option<T>::unwrap` -> T).
    Arg(usize),
    /// Returns `Wrapper<receiver's n-th type argument>`.
    Wrap(&'static str, usize),
    /// Returns a fixed, named external type.
    Named(&'static str),
    /// Returns `Wrapper<receiver's type arguments>`.
    WrapAll(&'static str),
}

/// What a std adapter's closure parameter is bound to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClosureArg {
    /// An element of the receiver (iterator item, `Option`/`Result` payload).
    Elem,
    /// A variant payload of the receiver (`map_err(|e| ..)`: `Err`).
    Variant(&'static str),
}

/// Module structure of a set of files.
#[derive(Clone, Debug, Default)]
pub struct Layout {
    /// For each file (by index in the slice given to [`Lang::layout`]), the
    /// `(file, scope)` that declares it as a child module, if any.
    pub parent_of: Vec<Option<(usize, u32)>>,
    /// Importable root name (crate / package) -> root file index.
    pub crates: HashMap<String, usize>,
    /// Directory modules for files whose declaring module is not part of
    /// the input (a partial file set): `(name, parent dir)`; entry 0 is the
    /// root. Such a file's parent directory is `orphan_dir[file]`.
    pub dirs: Vec<(String, Option<usize>)>,
    pub orphan_dir: Vec<Option<(usize, String)>>,
    /// Files whose top-level items live in their directory's scope, shared
    /// by the package (every Go file; a Python `__init__.py`).
    pub pooled: Vec<bool>,
    /// Importable name (import path) -> directory module index.
    pub dir_crates: HashMap<String, usize>,
}

impl Layout {
    /// Attach `orphans` (file index, path) to directory modules mirroring
    /// their paths, so `crate::a::b` / `super::b` still resolve when the
    /// files that declare `mod a;` are not part of the input.
    pub fn add_directory_modules(&mut self, orphans: &[(usize, &str)], n_files: usize) {
        self.orphan_dir = vec![None; n_files];
        if orphans.is_empty() {
            return;
        }
        let mut index: HashMap<String, usize> = HashMap::default();
        self.dirs.push((String::new(), None));
        index.insert(String::new(), 0);
        for &(fi, path) in orphans {
            let mut parent = 0;
            let mut prefix = String::new();
            let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
            for seg in dir.split('/').filter(|s| !s.is_empty()) {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(seg);
                let dirs = &mut self.dirs;
                parent = *index.entry(prefix.clone()).or_insert_with(|| {
                    dirs.push((seg.to_string(), Some(parent)));
                    dirs.len() - 1
                });
            }
            let file = path.rsplit('/').next().unwrap_or(path);
            let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
            self.orphan_dir[fi] = Some((parent, stem.to_string()));
        }
    }
}

pub trait Lang: Sync {
    fn lower(&self, tree: &tree_sitter::Tree, src: &str) -> FileFacts;
    fn layout(&self, root: &FsPath, files: &[(&str, &FileFacts)]) -> Layout;
    fn builtin_method(&self, recv: &str, method: &str) -> Option<BuiltinRet>;
    /// The type of a field of a std type (`Range<T>::start` is `T`).
    fn builtin_field(&self, ty: &str, field: &str) -> Option<BuiltinRet>;
    /// External types that auto-dereference to another type for method and
    /// field lookup (`Box<T>` -> `T`, `String` -> `str`).
    fn deref(&self, ty: &str) -> Option<BuiltinRet>;
    /// What a pattern on a standard-library variant binds (`Some(x)` binds
    /// the Option's argument 0).
    fn variant_payload(&self, ctor: &str, field: &str) -> Option<BuiltinRet>;
    /// What parameter `k` of a closure passed as argument `pos` to std
    /// method `method` is bound to (repo callees use their signatures).
    fn closure_param(&self, method: &str, pos: usize, k: usize) -> Option<ClosureArg>;
    /// The element type of a std container (`Vec<T>` -> T, Go `map[K]V` -> V).
    fn elem(&self, container: &str) -> Option<BuiltinRet>;
    /// Methods are virtual (Python): a call of a base class's method may
    /// run a subclass's override, so each gets a dispatch edge.
    fn virtual_methods(&self) -> bool {
        false
    }
    /// A base class from outside the repo that defines no ordinary methods
    /// (Python's `Generic`, `ABC`): it cannot shadow a repo base's.
    fn neutral_base(&self, _ty: &str) -> bool {
        false
    }
    /// The std iterator trait, its item type, and the std type modelling an
    /// iterator (Rust: `Iterator`, `Item`, `Iter`): a repo type implementing
    /// the trait iterates its item and has the std adapters.
    fn iterates_as(&self) -> Option<(&'static str, &'static str, &'static str)> {
        None
    }
    /// What calling a value of a std type returns (Python `type[X]` -> X).
    fn call_result(&self, _ty: &str) -> Option<BuiltinRet> {
        None
    }
    /// Interfaces are satisfied structurally (Go): dispatch edges go to every
    /// type whose methods cover the interface's.
    fn structural_interfaces(&self) -> bool {
        false
    }
    /// A name is one variable for a whole function, so every binding of it
    /// that can reach a use counts (Python); otherwise a binding is a new
    /// variable (`let` shadowing) or keeps its declared type (Go).
    fn function_scoped_names(&self) -> bool {
        false
    }
    /// Unannotated parameters are typed from call sites that all agree
    /// (a second resolution pass; dynamic languages).
    fn infer_params_from_calls(&self) -> bool {
        false
    }
    /// Base types are searched in declaration order (Python's MRO), so a
    /// base outside the repo matters only when it precedes a hit; otherwise
    /// (Go embedding) any such base may be the shallower one.
    fn ordered_bases(&self) -> bool {
        false
    }
    /// The receiver name inside methods (`self`, `this`).
    fn self_value(&self) -> &'static str;
    /// The name of the enclosing impl's type (`Self`).
    fn self_type(&self) -> &'static str;
}
