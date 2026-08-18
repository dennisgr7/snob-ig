//! Guards the prose this repository ships.
//!
//! Two things live here, and they share a walk of the source tree because that
//! walk is the awkward part. A test rather than a script because that is the
//! only thing that runs on every `cargo test`, on every machine and in CI, with
//! nothing to install.
//!
//! **Spanish creeping back in.** The project was translated to English in July
//! 2026. Three detectors, because each covers the others' blind spot. An escape
//! hatch exists: any line containing `i18n-allow` is skipped.
//!
//! **Runs of spaces left in the middle of a sentence.** Cosmetic, and it earned
//! its place anyway: the same defect shipped twice in `snob watch status`, and
//! the second time it arrived *in the commit whose message said it was fixed*
//! and four spaces longer than before. A multi-line string written with a `\`
//! continuation collapses correctly until `cargo fmt` joins the line, and then
//! the indentation is suddenly part of what the user reads. Two rounds of
//! reading missed it. Its escape hatch is `layout-allow`.

use std::path::{Path, PathBuf};

/// Characters that only Spanish uses. Cheap and very high signal.
const SPANISH_CHARS: [char; 15] = [
    'á', 'é', 'í', 'ó', 'ú', 'Á', 'É', 'Í', 'Ó', 'Ú', 'ñ', 'Ñ', '¿', '¡', '»',
];

/// Morphological endings. This is the one that ages well, because it does not
/// depend on having guessed the right words. `-tion` is not `-cion`, and
/// `-mente`, `-miento` and `-iendo` do not occur in English.
const SPANISH_SUFFIXES: [&str; 13] = [
    "cion", "ciones", "miento", "mientos", "mente", "ando", "iendo", "ados", "adas", "idos",
    "idas", "aron", "aban",
];

/// English words that end like a Spanish one and would otherwise be flagged.
const SUFFIX_EXCEPTIONS: [&str; 8] = [
    "commando",
    "commands",
    "avocados",
    "tornados",
    "desperados",
    "aficionados",
    // English almost always spells this ending "-tion" or "-sion". These two
    // are the ones that do not, and they turn up in ordinary prose.
    "suspicion",
    "coercion",
];

/// Domain and high-frequency words.
///
/// **No homographs here, ever.** A single `red`, `fin`, `sin`, `base`, `real`
/// or `error` in this list turns the test into permanent noise. Over-pruning
/// only causes a false negative, which is the safe direction: the other two
/// detectors catch nearly everything that slips through.
///
/// Two were in it anyway. `leer` is an ordinary English verb, and `todos`
/// arrives from every `TODO` written in the plural -- `words_of` splits on
/// non-alphabetics and `spanish_in` lowercases first, so `TODOs` in any `.rs`,
/// `.md` or `.yml` reached this list and failed the build over correct English.
/// Both are still caught in their accented forms by `SPANISH_CHARS`, and a real
/// relapse into Spanish trips several of the words that remain.
const SPANISH_WORDS: [&str; 44] = [
    "motivo",
    "aviso",
    "instantanea",
    "presupuesto",
    "recorrido",
    "ritmo",
    "cancelacion",
    "desenlace",
    "procedencia",
    "entorno",
    "usuario",
    "usuarios",
    "cuenta",
    "cuentas",
    "seguidores",
    "seguidos",
    "peticion",
    "peticiones",
    "pagina",
    "paginas",
    "consulta",
    "fichero",
    "archivo",
    "carpeta",
    "llavero",
    "enfriamiento",
    "espera",
    "guardar",
    "borrar",
    "buscar",
    "escribir",
    "porque",
    "aunque",
    "mientras",
    "tambien",
    "cuando",
    "donde",
    "siempre",
    "nunca",
    "hasta",
    "desde",
    "entre",
    "mejor",
    "puede",
];

/// Old names, so copy-pasted stale code is caught. Zero false positives by
/// construction; can be dropped after a release.
const OLD_NAMES: [&str; 13] = [
    "Motivo",
    "Instantanea",
    "Presupuesto",
    "Recorrido",
    "Ritmo",
    "Cancelacion",
    "Desenlace",
    "Procedencia",
    "Entorno",
    "snapshots_utiles",
    "emision_ms",
    "rafaga_ms",
    "SNOB_IGNORAR_ENFRIAMIENTO",
];

/// Files exempt from the check.
///
/// Note that this walks the working directory rather than the git index, so a
/// file that is untracked still gets checked and still has to be listed here.
const ALLOWLIST: [&str; 1] = [
    // This very file holds the Spanish word list, so it would flag itself. The
    // trap everyone forgets.
    "crates/snob-core/tests/language.rs",
];

/// The check is only worth as much as the files it reaches. Packaging brought
/// in shell, PowerShell, Ruby and JSON, and the manifests are `.yaml` where the
/// workflows are `.yml`; without these the guard had holes exactly where the
/// newest text was being written.
const EXTENSIONS: [&str; 10] = [
    "rs", "sql", "md", "toml", "yml", "yaml", "sh", "ps1", "rb", "json",
];

#[test]
fn no_spanish_is_left_in_the_repository() {
    let Some(root) = repo_root() else {
        return; // packaged build, nothing to walk
    };

    let mut violations = Vec::new();
    for file in source_files(&root) {
        let relative = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");

        if ALLOWLIST.contains(&relative.as_str()) {
            continue;
        }

        let Ok(contents) = std::fs::read_to_string(&file) else {
            continue;
        };

        for (number, line) in contents.lines().enumerate() {
            if line.contains("i18n-allow") {
                continue;
            }
            if let Some(found) = spanish_in(line) {
                violations.push(format!("{relative}:{}: {found}", number + 1));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Spanish found in {} place(s):\n{}",
        violations.len(),
        violations.join("\n")
    );
}

/// Returns the first offending token in the line, if any.
fn spanish_in(line: &str) -> Option<String> {
    if let Some(c) = line.chars().find(|c| SPANISH_CHARS.contains(c)) {
        return Some(format!("Spanish character '{c}'"));
    }

    for name in OLD_NAMES {
        if line.contains(name) {
            return Some(format!("old name \"{name}\""));
        }
    }

    for word in words_of(line) {
        let lower = word.to_lowercase();
        if lower.len() < 4 || SUFFIX_EXCEPTIONS.contains(&lower.as_str()) {
            continue;
        }
        if SPANISH_WORDS.contains(&lower.as_str()) {
            return Some(format!("Spanish word \"{word}\""));
        }
        if lower.len() >= 6 && SPANISH_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
            return Some(format!("Spanish ending in \"{word}\""));
        }
    }

    None
}

/// Splits into words, treating `_` as a boundary so `read_list_of_names` is
/// checked piece by piece.
fn words_of(line: &str) -> impl Iterator<Item = &str> {
    line.split(|c: char| !c.is_ascii_alphabetic())
        .filter(|w| !w.is_empty())
}

/// A run of three or more spaces in the middle of a sentence.
///
/// Bounded on both sides on purpose, because the shape of the defect is what
/// tells it apart from deliberate layout. **Before** it: a lowercase letter,
/// which ends a word -- not the `:` that precedes every aligned label in this
/// repository, and not the `,` that precedes an aligned SQL argument.
/// **After** it: a lowercase letter or `{`, which is a word or a format
/// placeholder beginning, and not the uppercase that follows every aligned SQL
/// keyword (`JOIN users u            ON ...`).
///
/// That leaves one shape it cannot tell apart, and it is allowlisted below
/// rather than guessed at.
fn stray_spaces_in(line: &str) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != ' ' {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && chars[i] == ' ' {
            i += 1;
        }
        if i - start < 3 || start == 0 || i == chars.len() {
            continue;
        }
        let before = chars[start - 1];
        let after = chars[i];
        if before.is_ascii_lowercase() && (after.is_ascii_lowercase() || after == '{') {
            return Some(format!("{} spaces mid-sentence", i - start));
        }
    }
    None
}

/// Files exempt from the space check.
const LAYOUT_ALLOWLIST: [&str; 2] = [
    // This very file holds the examples, so it flags itself -- the same trap
    // the Spanish allowlist above records, for the same reason.
    "crates/snob-core/tests/language.rs",
    // The long help is a two-column table written inside one string literal,
    // so `login                          store your session` has exactly the
    // shape of the defect. The cost of not writing a cleverer detector is that
    // this one file is not watched for it.
    "crates/snob-cli/src/cli.rs",
];

/// No sentence this repository ships has a hole punched in it.
#[test]
fn no_run_of_spaces_is_left_inside_a_sentence() {
    let Some(root) = repo_root() else {
        return; // packaged build, nothing to walk
    };

    let mut violations = Vec::new();
    for file in source_files(&root) {
        if file.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let relative = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");

        if LAYOUT_ALLOWLIST.contains(&relative.as_str()) {
            continue;
        }

        let Ok(contents) = std::fs::read_to_string(&file) else {
            continue;
        };

        for (number, line) in contents.lines().enumerate() {
            if line.contains("layout-allow") {
                continue;
            }
            if let Some(found) = stray_spaces_in(line) {
                violations.push(format!("{relative}:{}: {found}", number + 1));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "{} sentence(s) with a run of spaces in them:\n{}",
        violations.len(),
        violations.join("\n")
    );
}

/// Walks up from the manifest until a `Cargo.lock` shows up.
fn repo_root() -> Option<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|d| d.join("Cargo.lock").is_file())
        .map(Path::to_path_buf)
}

/// Directories this never descends into.
///
/// `target` and `.git` are the obvious ones. The rest are where a developer's
/// own files live, and since `json` joined the extension list the walk reaches
/// things that are nobody's source: an editor's settings, and — the one that
/// matters — an Instagram data export or a `snob lists -o out.json` written
/// from the repository root. Those are full of real names with real accents,
/// so the test would fail on them **and print them into the assertion
/// message**. That is the "permanent noise and someone switches it off"
/// outcome the language rule warns about, arriving with someone else's
/// personal data attached.
const SKIP_DIRS: [&str; 6] = ["target", ".git", ".claude", ".vscode", ".idea", "exports"];

fn source_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];

    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if path.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    pending.push(path);
                }
            } else if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| EXTENSIONS.contains(&e))
            {
                found.push(path);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::{spanish_in, stray_spaces_in};

    #[test]
    fn it_catches_the_three_kinds_of_giveaway() {
        assert!(spanish_in("// la paginación").is_some());
        assert!(spanish_in("let desplazamiento = 1;").is_some());
        assert!(spanish_in("// esto es un fichero").is_some());
        assert!(spanish_in("struct Motivo;").is_some());
    }

    /// The homographs it must never flag. If any of these trips, the test
    /// becomes noise and someone will disable it.
    #[test]
    fn it_does_not_flag_ordinary_english() {
        for line in [
            "let error = red_channel();",
            "// the final base value is normal",
            "fn find(&self) -> Option<User>",
            "// this is a real, simple, general purpose comment",
            "let total = sin(x) + cos(y);",
            "// no data, so the result is None",
            "pub const HARD_PAGE_CAP: u32 = 2_000;",
            "// commands are parsed here",
            "// the walk is resumed from the stored cursor",
            "// checking whether the counter moved",
        ] {
            assert!(spanish_in(line).is_none(), "false positive on: {line}");
        }
    }

    #[test]
    fn a_hole_punched_in_a_sentence_is_found() {
        // The two that shipped, in the shape they shipped in.
        assert!(stray_spaces_in("so nothing will send      {it}. They expire").is_some());
        assert!(stray_spaces_in("says nothing about      having served").is_some());
    }

    #[test]
    fn deliberate_layout_is_not_a_hole() {
        for line in [
            // A label column: the `:` is what says so.
            r#"format!("Origin:   {}", session.origin)"#,
            r#"text.contains("Account:      @someone")"#,
            // Aligned SQL: the keyword after it is uppercase.
            r#""JOIN users u            ON u.pk = h.pk""#,
            // Aligned SQL: neither the `=` nor the `,` before a column name
            // ends a word.
            r#""full_name   = coalesce(excluded.full_name,   users.full_name)""#,
            // Ordinary code and ordinary prose.
            "let total = sin(x) + cos(y);",
            "// the walk is resumed from the stored cursor",
            "        indented(code);",
        ] {
            assert!(stray_spaces_in(line).is_none(), "false positive on: {line}");
        }
    }

    #[test]
    fn the_escape_hatch_is_honored_by_the_caller() {
        // The line filter lives in the walker; here we only check the detector
        // would otherwise have fired.
        assert!(spanish_in("valor = 'usuario' // i18n-allow").is_some());
    }
}
