//! File classification: category by extension, secret detection, and which
//! directories are never descended into.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Dir,
    Image,
    Video,
    Audio,
    Pdf,
    Document,
    Presentation,
    Spreadsheet,
    Code,
    Text,
    Archive,
    Cad,
    Design,
    Other,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Dir => "dir",
            Category::Image => "image",
            Category::Video => "video",
            Category::Audio => "audio",
            Category::Pdf => "pdf",
            Category::Document => "document",
            Category::Presentation => "presentation",
            Category::Spreadsheet => "spreadsheet",
            Category::Code => "code",
            Category::Text => "text",
            Category::Archive => "archive",
            Category::Cad => "cad",
            Category::Design => "design",
            Category::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Category> {
        Some(match s {
            "dir" => Category::Dir,
            "image" => Category::Image,
            "video" => Category::Video,
            "audio" => Category::Audio,
            "pdf" => Category::Pdf,
            "document" => Category::Document,
            "presentation" => Category::Presentation,
            "spreadsheet" => Category::Spreadsheet,
            "code" => Category::Code,
            "text" => Category::Text,
            "archive" => Category::Archive,
            "cad" => Category::Cad,
            "design" => Category::Design,
            "other" => Category::Other,
            _ => return None,
        })
    }

    /// Media whose date in a query is a hard filter on capture time.
    pub fn is_media(self) -> bool {
        matches!(self, Category::Image | Category::Video)
    }
}

/// Lower-cased extension of a raw file name, without the dot. Empty if none.
pub fn extension(name: &[u8]) -> String {
    match name.iter().rposition(|b| *b == b'.') {
        Some(0) | None => String::new(),
        Some(i) => {
            let ext = &name[i + 1..];
            if ext.len() > 12 {
                return String::new();
            }
            String::from_utf8_lossy(ext).to_ascii_lowercase()
        }
    }
}

pub fn category_for(name: &[u8]) -> Category {
    let ext = extension(name);
    let lower = String::from_utf8_lossy(name).to_ascii_lowercase();
    match lower.as_str() {
        "makefile" | "dockerfile" | "containerfile" | "justfile" | "rakefile" | "gemfile"
        | "cmakelists.txt" | "vagrantfile" | "jenkinsfile" => return Category::Code,
        "readme" | "license" | "copying" | "authors" | "changelog" | "todo" | "notes" => {
            return Category::Text
        }
        _ => {}
    }
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" | "tif" | "tiff" | "heic" | "heif"
        | "avif" | "raw" | "cr2" | "cr3" | "nef" | "arw" | "dng" | "raf" | "orf" | "rw2" => {
            Category::Image
        }
        "mp4" | "mov" | "mkv" | "webm" | "avi" | "m4v" | "3gp" | "wmv" | "flv" | "mts" => {
            Category::Video
        }
        "mp3" | "flac" | "ogg" | "opus" | "wav" | "m4a" | "aac" | "wma" | "aiff" => {
            Category::Audio
        }
        "pdf" => Category::Pdf,
        "docx" | "doc" | "odt" | "rtf" | "epub" | "pages" => Category::Document,
        "pptx" | "ppt" | "odp" | "key" => Category::Presentation,
        "xlsx" | "xls" | "ods" | "csv" | "tsv" | "numbers" => Category::Spreadsheet,
        "rs" | "py" | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "go" | "c" | "h" | "cc"
        | "cpp" | "cxx" | "hpp" | "hh" | "java" | "kt" | "kts" | "swift" | "rb" | "php"
        | "cs" | "fs" | "scala" | "lua" | "pl" | "pm" | "r" | "jl" | "dart" | "zig" | "nim"
        | "hs" | "ml" | "ex" | "exs" | "erl" | "clj" | "sh" | "bash" | "zsh" | "fish"
        | "ps1" | "bat" | "sql" | "tf" | "tfvars" | "hcl" | "nix" | "vim" | "el" | "cmake"
        | "gradle" | "proto" | "graphql" | "vue" | "svelte" | "css" | "scss" | "sass"
        | "less" | "html" | "htm" | "xml" | "yaml" | "yml" | "toml" | "json" | "jsonc"
        | "ini" | "cfg" | "conf" | "ipynb" | "mk" | "asm" | "s" | "v" | "sv" | "vhd"
        | "glsl" | "hlsl" | "wgsl" | "cu" => Category::Code,
        "txt" | "md" | "markdown" | "rst" | "org" | "adoc" | "tex" | "log" | "srt" | "vtt"
        | "nfo" => Category::Text,
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "iso" | "deb"
        | "rpm" | "appimage" => Category::Archive,
        "step" | "stp" | "iges" | "igs" | "stl" | "dxf" | "dwg" | "fcstd" | "scad" | "blend"
        | "3mf" | "obj" | "fbx" | "gltf" | "glb" | "skp" | "f3d" | "kicad_pcb" | "kicad_sch"
        | "kicad_pro" | "sldprt" | "sldasm" => Category::Cad,
        "psd" | "xcf" | "kra" | "ai" | "sketch" | "fig" | "xd" | "svg" | "afdesign"
        | "afphoto" | "indd" | "drawio" => Category::Design,
        _ => Category::Other,
    }
}

/// Categories whose text the extractor knows how to pull out.
pub fn has_extractable_text(cat: Category, ext: &str) -> bool {
    match cat {
        Category::Text | Category::Code | Category::Pdf => true,
        Category::Document => matches!(ext, "docx" | "odt"),
        Category::Presentation => matches!(ext, "pptx" | "odp"),
        Category::Spreadsheet => matches!(ext, "xlsx" | "ods" | "csv" | "tsv"),
        Category::Design => ext == "svg",
        Category::Cad => matches!(ext, "dxf" | "step" | "stp" | "scad" | "kicad_sch" | "kicad_pcb"),
        _ => false,
    }
}

/// Image formats the extractor can decode for visual embedding.
pub fn has_decodable_image(ext: &str) -> bool {
    matches!(ext, "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" | "tif" | "tiff")
}

/// Files whose *content* must never be read, embedded or shown to an agent.
/// Their names are still indexed so they can be found by name.
pub fn is_secret(name: &[u8]) -> bool {
    let n = String::from_utf8_lossy(name).to_ascii_lowercase();
    const EXACT_PREFIX: &[&str] = &["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519", ".env", ".netrc", ".pgpass", "known_hosts", "authorized_keys"];
    const EXTS: &[&str] = &[
        "pem", "key", "p12", "pfx", "jks", "keystore", "kdbx", "kdb", "gpg", "pgp", "secret",
        "secrets", "env", "ovpn", "ppk", "age", "tfstate",
    ];
    const WORDS: &[&str] = &[
        "password", "passwd", "credential", "secret", "access_token", "auth_token", "apikey", "api_key",
        "private_key", "privkey", "wallet",
    ];
    if EXACT_PREFIX.iter().any(|p| n.starts_with(p)) {
        return true;
    }
    if EXTS.contains(&extension(name).as_str()) {
        return true;
    }
    WORDS.iter().any(|w| n.contains(w))
}

/// Directory names never descended into (generated, cached or system data).
pub fn is_skipped_dir_name(name: &[u8]) -> bool {
    if name.first() == Some(&b'.') {
        // Hidden directories: dotfiles, .git, .cache, .local, .config, ...
        // Power-user mode may opt specific ones back in later.
        return true;
    }
    matches!(
        name,
        b"node_modules"
            | b"__pycache__"
            | b"site-packages"
            | b"bower_components"
            | b"snap"
            | b"lost+found"
    )
}

/// Marker files that exclude the directory containing them (and its subtree).
/// `CACHEDIR.TAG` is the cross-tool standard (cargo `target/` writes one).
pub const EXCLUDE_MARKERS: &[&str] = &[".noindex", ".cairnignore", "CACHEDIR.TAG", ".nomedia", "pyvenv.cfg"];

/// Files inside a folder that describe that folder (user-authored context).
pub fn is_context_file(name: &[u8]) -> bool {
    let n = String::from_utf8_lossy(name).to_ascii_lowercase();
    if n.starts_with("readme") || n.starts_with("notes") || n.starts_with("about") || n.starts_with("description") {
        return true;
    }
    let ext = extension(name);
    matches!(ext.as_str(), "txt" | "md" | "markdown" | "org" | "rst")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories() {
        assert_eq!(category_for(b"IMG_0001.JPG"), Category::Image);
        assert_eq!(category_for(b"main.tf"), Category::Code);
        assert_eq!(category_for(b"Makefile"), Category::Code);
        assert_eq!(category_for(b"report.pdf"), Category::Pdf);
        assert_eq!(category_for(b".bashrc"), Category::Other);
        assert_eq!(category_for(b"noext"), Category::Other);
    }

    #[test]
    fn secrets() {
        for s in ["id_rsa", "id_ed25519.pub", "db-ca.pem", "redis.secret", "app-db-passwords.env", ".env.local", "aws_credentials", "vault.kdbx"] {
            assert!(is_secret(s.as_bytes()), "{s}");
        }
        for s in ["report.pdf", "notes.md", "main.rs", "tokenizer.json"] {
            assert!(!is_secret(s.as_bytes()), "{s}");
        }
    }

    #[test]
    fn non_utf8_names_do_not_panic() {
        let name = b"caf\xe9.txt";
        assert_eq!(extension(name), "txt");
        assert_eq!(category_for(name), Category::Text);
        assert!(!is_secret(name));
    }
}
