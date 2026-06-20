/// File names that are excluded from repo-wide scans by default.
const DEFAULT_EXCLUDED_FILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Gemfile.lock",
    "Pipfile.lock",
    "poetry.lock",
    "composer.lock",
    "go.sum",
    "flake.lock",
];

/// Directory names excluded wherever they appear in repo-wide scans.
/// This list includes generated artifacts and high-volume support trees that are
/// usually not useful for entity lookup or impact analysis by default.
const DEFAULT_EXCLUDED_ANY_DIRS: &[&str] = &[
    "__generated__",
    "_generated",
    "generated",
    "fixtures",
    "fixture",
    "benchmarks",
    "vendor",
    "node_modules",
    "test-harness",
    ".next",
    ".turbo",
    ".cache",
    "coverage",
];

/// Top-level output directories excluded by default.
const DEFAULT_EXCLUDED_ROOT_DIRS: &[&str] = &["out", "dist", "build", "target"];

/// File suffixes for generated text assets that do not produce useful entities.
const DEFAULT_EXCLUDED_SUFFIXES: &[&str] = &[
    ".min.js",
    ".min.css",
    ".generated.ts",
    ".generated.tsx",
    ".generated.js",
    ".generated.jsx",
    ".generated.mts",
    ".generated.cts",
    ".generated.mjs",
    ".generated.cjs",
    ".generated.d.ts",
    ".gen.ts",
    ".gen.tsx",
    ".gen.js",
    ".gen.jsx",
    ".gen.mts",
    ".gen.cts",
    ".gen.mjs",
    ".gen.cjs",
    ".gen.d.ts",
    ".module.css.d.ts",
    ".module.scss.d.ts",
    ".module.sass.d.ts",
    ".module.less.d.ts",
    ".svg.d.ts",
    ".png.d.ts",
    ".jpg.d.ts",
    ".jpeg.d.ts",
    ".webp.d.ts",
    ".gif.d.ts",
    ".avif.d.ts",
    ".ico.d.ts",
];

/// File suffixes that are not useful source text for semantic extraction.
const BINARY_FILE_SUFFIXES: &[&str] = &[
    ".png",
    ".jpg",
    ".jpeg",
    ".gif",
    ".webp",
    ".ico",
    ".tiff",
    ".tif",
    ".bmp",
    ".heic",
    ".heif",
    ".avif",
    ".woff",
    ".woff2",
    ".ttf",
    ".otf",
    ".eot",
    ".mp3",
    ".mp4",
    ".mov",
    ".avi",
    ".webm",
    ".ogg",
    ".wav",
    ".flac",
    ".m4a",
    ".m4v",
    ".mkv",
    ".zip",
    ".tar",
    ".tar.gz",
    ".tgz",
    ".bz2",
    ".gz",
    ".xz",
    ".7z",
    ".rar",
    ".so",
    ".dylib",
    ".dll",
    ".a",
    ".lib",
    ".o",
    ".obj",
    ".class",
    ".jar",
    ".pyc",
    ".pyo",
    ".pdb",
    ".exe",
    ".app",
    ".apk",
    ".ipa",
    ".aar",
    ".swiftmodule",
    ".swiftdoc",
    ".swiftsourceinfo",
    ".wasm",
    ".car",
    ".icns",
    ".riv",
    ".pdf",
    ".nib",
    ".storyboardc",
    ".db",
    ".sqlite",
    ".sqlite3",
    ".realm",
    ".profdata",
];

/// Directory/package suffixes that contain compiled assets rather than source.
const BINARY_DIR_SUFFIXES: &[&str] = &[".framework", ".xcframework", ".dsym", ".app"];

pub fn is_default_excluded(rel_path: &str) -> bool {
    let normalized = rel_path.replace('\\', "/");
    let lower = normalized.to_ascii_lowercase();

    if let Some(file_name) = lower.rsplit('/').next() {
        if DEFAULT_EXCLUDED_FILES
            .iter()
            .any(|excluded| excluded.eq_ignore_ascii_case(file_name))
        {
            return true;
        }
    }

    if DEFAULT_EXCLUDED_SUFFIXES
        .iter()
        .any(|suffix| lower.ends_with(suffix))
    {
        return true;
    }

    let components: Vec<&str> = lower.split('/').collect();
    if components
        .iter()
        .any(|component| DEFAULT_EXCLUDED_ANY_DIRS.contains(component))
    {
        return true;
    }

    if components
        .first()
        .is_some_and(|component| DEFAULT_EXCLUDED_ROOT_DIRS.contains(component))
    {
        return true;
    }

    components
        .windows(3)
        .any(|window| window == ["_next", "static", "chunks"])
}

pub fn is_probably_binary_path(rel_path: &str) -> bool {
    let normalized = rel_path.replace('\\', "/");
    let lower = normalized.to_ascii_lowercase();

    if BINARY_FILE_SUFFIXES
        .iter()
        .any(|suffix| lower.ends_with(suffix))
    {
        return true;
    }

    lower.split('/').any(|component| {
        BINARY_DIR_SUFFIXES
            .iter()
            .any(|suffix| component.ends_with(suffix))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_excludes_generated_support_and_build_paths() {
        assert!(is_default_excluded("dist/app.js"));
        assert!(is_default_excluded("site/out/_next/static/chunks/app.js"));
        assert!(is_default_excluded("src/generated.min.js"));
        assert!(is_default_excluded("src/__generated__/client.ts"));
        assert!(is_default_excluded("src/_generated/tokens.ts"));
        assert!(is_default_excluded("src/generated/schema.ts"));
        assert!(is_default_excluded("src/api.generated.ts"));
        assert!(is_default_excluded("src/api.generated.d.ts"));
        assert!(is_default_excluded(
            "src/components/Button.module.scss.d.ts"
        ));
        assert!(is_default_excluded("src/icons/logo.svg.d.ts"));
        assert!(is_default_excluded("src/fixtures/example.ts"));
        assert!(is_default_excluded("src/fixture/example.ts"));
        assert!(is_default_excluded("packages/app/benchmarks/run.ts"));
        assert!(is_default_excluded("packages/app/vendor/client.ts"));
        assert!(is_default_excluded("tools/test-harness/main.ts"));
        assert!(is_default_excluded("target/debug/build.rs"));
        assert!(!is_default_excluded("src/app.js"));
        assert!(!is_default_excluded("src/generated_value.ts"));
        assert!(!is_default_excluded("src/build/mod.rs"));
        assert!(!is_default_excluded("packages/compiler/build/index.ts"));
        assert!(!is_default_excluded("src/cli/commands/codegen/run.ts"));
        assert!(!is_default_excluded("tools/dist/analyzer.py"));
    }

    #[test]
    fn detects_binary_asset_paths() {
        assert!(is_probably_binary_path("Snapshots/icon.png"));
        assert!(is_probably_binary_path("Frameworks/Foo.framework/Foo"));
        assert!(is_probably_binary_path("Modules/Foo.swiftmodule"));
        assert!(!is_probably_binary_path("Assets/icon.svg"));
        assert!(!is_probably_binary_path("src/main.rs"));
    }
}
