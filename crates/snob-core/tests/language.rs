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

mod common;
use common::{relative, repo_root, source_files};

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
    for file in source_files(&root, &EXTENSIONS) {
        let relative = relative(&root, &file);

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

/// British spellings of words this project writes the US way.
///
/// AGENTS.md has said "US spelling. A test enforces it" since the translation,
/// and no test did — only the Spanish half was ever checked. Thirty-six of
/// these had accumulated across twenty-four files, including AGENTS.md itself
/// and a shipped SQL migration, against far more numerous US forms of the same
/// words: `canceled` 63 against `cancelled` 4, `behavior` 9 against `behaviour`
/// 5. `pace.rs` contradicted itself twice inside two hundred lines.
///
/// Longer forms come first, so `neighbouring` is reported as itself rather than
/// as `neighbour`. **`cancellation` is deliberately absent**: it is spelled
/// with two Ls in US English too, and there are some twenty correct uses of it
/// in the tree — putting it here would turn this guard into noise, which is how
/// a guard gets switched off.
const BRITISH: [&str; 17] = [
    "ageing",
    "behaviour",
    "cancelling",
    "cancelled",
    "colour",
    "honoured",
    "judgement",
    "labelled",
    "licence",
    "neighbouring",
    "neighbours",
    "neighbour",
    "recognising",
    "recognised",
    "recognise",
    "travelling",
    "travelled",
];

#[test]
fn us_spelling_is_what_the_repository_writes() {
    let Some(root) = repo_root() else {
        return; // packaged build, nothing to walk
    };

    let mut violations = Vec::new();
    for file in source_files(&root, &EXTENSIONS) {
        let relative = relative(&root, &file);

        // This file holds the word list, for the reason the Spanish allowlist
        // gives about itself.
        if ALLOWLIST.contains(&relative.as_str()) {
            continue;
        }

        let Ok(contents) = std::fs::read_to_string(&file) else {
            continue;
        };

        for (number, line) in contents.lines().enumerate() {
            if line.contains("spelling-allow") {
                continue;
            }
            if let Some(found) = british_in(line) {
                violations.push(format!("{relative}:{}: {found}", number + 1));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "British spelling found in {} place(s):
{}",
        violations.len(),
        violations.join(
            "
"
        )
    );
}

/// The first British spelling in the line, if any.
///
/// Whole words only, so `licence` does not fire on a hypothetical `licenced`
/// that is not in the list, and — the case that matters — nothing fires on a
/// longer word that merely contains one of these.
fn british_in(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    BRITISH
        .iter()
        .find(|word| whole_word(&lower, word))
        .map(|word| (*word).to_string())
}

/// Whether `needle` occurs in `haystack` bounded by non-letters on both sides.
fn whole_word(haystack: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(at) = haystack[from..].find(needle) {
        let start = from + at;
        let end = start + needle.len();
        let before_ok = start == 0
            || !haystack[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphabetic());
        let after_ok = !haystack[end..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic());
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

/// The guard has to be able to fail, and it has to leave `cancellation` alone.
#[test]
fn the_spelling_guard_knows_a_word_from_a_word_that_contains_one() {
    assert_eq!(
        british_in("the walk was cancelled"),
        Some("cancelled".into())
    );
    assert_eq!(
        british_in("neighbouring pages"),
        Some("neighbouring".into()),
        "the longer form names itself"
    );
    assert_eq!(
        british_in("Behaviour at the edge"),
        Some("behaviour".into())
    );

    assert_eq!(
        british_in("cancellation is spelled this way in both"),
        None,
        "two Ls is correct US English and there are twenty of these"
    );
    assert_eq!(british_in("the walk was canceled"), None);
    assert_eq!(
        british_in("licenced"),
        None,
        "whole words only, or the list has to grow a form for every suffix"
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
        // **A comment and a string literal are judged differently, because
        // only one of them can be wrong in a way that reaches anybody.**
        //
        // In prose, two lowercase letters either side of a run is the whole
        // signal: a doc comment is full of deliberate indentation — bullet
        // lists, tables, aligned continuations — and widening the rule there
        // reports dozens of them.
        //
        // In a string literal there is no deliberate indentation, so any two
        // non-space characters count. The narrow rule let three real ones
        // through, all between `-` and a quote: two signing-secret messages a
        // user reads, and a seventeen-space run inside the `Accept` header for
        // a navigation, which went out on the wire before every write.
        //
        // The mechanism behind all four was the same and is worth knowing:
        // `cargo fmt` rejoins a `\`-continued string literal onto one line and
        // materializes the indentation as spaces inside the value. `concat!`
        // of separate literals is the form that survives it.
        //
        // The second clause is what the first one missed. All four real cases
        // sat between a comma or a dash and the next word or quote, which is
        // where a sentence continues and where a column never begins: SQL
        // alignment runs into an uppercase keyword, and a fixture's run sits
        // inside its own quotes.
        let mid_sentence =
            before.is_ascii_lowercase() && (after.is_ascii_lowercase() || after == '{');
        // The length is what separates the two. Deliberate alignment inside a
        // SQL string is three to five spaces wide; a run left by a rejoined
        // continuation is thirteen to thirty-three, because it is the source
        // file's indentation. Eight is comfortably between them.
        let after_a_break = i - start >= 8
            && matches!(before, ',' | '-')
            // The backslash is not decoration: in the two messages this was
            // written for, the run runs into an escaped quote, so what follows
            // it in the source is `\` and not `"`.
            && (after.is_ascii_lowercase() || after == '"' || after == '\\');
        if mid_sentence || after_a_break {
            return Some(format!("{} spaces mid-sentence", i - start));
        }
    }
    None
}

/// The four shapes the widened rule was written for, and the three it must
/// keep ignoring.
///
/// All four real ones came from the same mechanism: `cargo fmt` rejoins a
/// `\`-continued string literal onto one line and materializes the source
/// file's indentation as spaces inside the value. One of them was the `Accept`
/// header for a navigation, so it went out on the wire.
#[test]
fn a_rejoined_continuation_is_caught_and_deliberate_alignment_is_not() {
    for caught in [
        r#"    "text/html,application/xml;q=0.9,                 image/webp,*/*""#,
        r#"    "at least 32 characters. Generate one --              \"openssl rand -hex 32\"""#,
        r#"    "id, account_pk, kind,                                 declared_count""#,
        r#"    "a secret has to be at least {FLOOR} and this one is              {length}.""#,
    ] {
        assert!(
            stray_spaces_in(caught).is_some(),
            "missed a rejoined continuation: {caught}"
        );
    }
    for left_alone in [
        r#"             is_private  = coalesce(excluded.is_private,  users.is_private),"#,
        r#"        for input in ["", "   ", "noseparator"] {"#,
        "///   - a bullet in a doc comment, indented on purpose",
    ] {
        assert!(
            stray_spaces_in(left_alone).is_none(),
            "reported deliberate alignment: {left_alone}"
        );
    }
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
    for file in source_files(&root, &["rs"]) {
        let relative = relative(&root, &file);

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

/// A `snob …` command written to be typed back, carrying an at sign.
///
/// On PowerShell `@` is the splatting operator, so an unquoted `@someone` is
/// gone before `main` runs: the command answers about the reader's own account,
/// with exit 0 and nothing on screen to say a different question was asked.
/// Quoting it is the documented escape, but a name is accepted with or without
/// the sign, so a command that leaves it off needs no explanation next to it.
///
/// The command is read from `snob ` to whatever closes the literal it sits in —
/// a quote or a backtick — or to the end of the line. Prose around it is not
/// searched, which is what keeps `@someone` in the sentence *explaining* the
/// rule from tripping it.
fn at_name_in_a_command(line: &str) -> Option<String> {
    let mut rest = line;
    while let Some(at) = rest.find("snob ") {
        let after = &rest[at + "snob ".len()..];
        let command = after.split(['"', '`']).next().unwrap_or(after);
        if command.contains('@') {
            return Some(format!("snob {}", command.trim_end()));
        }
        rest = after;
    }
    None
}

/// Nothing this repository prints or documents hands over a command with an
/// unquoted `@name` in it.
#[test]
fn no_command_is_written_with_an_at_name_to_type_back() {
    let Some(root) = repo_root() else {
        return; // packaged build, nothing to walk
    };

    let mut violations = Vec::new();
    for file in source_files(&root, &EXTENSIONS) {
        let relative = relative(&root, &file);

        // This file holds the examples, so it flags itself -- the same trap
        // the two allowlists above record, for the same reason.
        if relative == "crates/snob-core/tests/language.rs" {
            continue;
        }

        let Ok(contents) = std::fs::read_to_string(&file) else {
            continue;
        };

        for (number, line) in contents.lines().enumerate() {
            if let Some(found) = at_name_in_a_command(line) {
                violations.push(format!("{relative}:{}: {found}", number + 1));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "{} command(s) a reader would copy and lose the name from:\n{}",
        violations.len(),
        violations.join("\n")
    );
}

#[cfg(test)]
mod tests {
    use super::{at_name_in_a_command, spanish_in, stray_spaces_in};

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

    /// The offender the guard was written for, in the shape it shipped in.
    #[test]
    fn a_command_that_hands_over_an_at_name_is_found() {
        assert!(
            at_name_in_a_command(r#"row.push_str("  for details, run \"snob fans @someone\"")"#)
                .is_some()
        );
        assert!(at_name_in_a_command("Run `snob followers @someone` once.").is_some());
        assert!(at_name_in_a_command("    snob unfollowers @someone").is_some());
    }

    /// The sentence that *explains* the rule names an at sign next to the word
    /// `snob` all over the documentation. Flagging those is how this becomes
    /// noise, so the command is read only as far as the literal it sits in.
    #[test]
    fn prose_around_a_command_is_not_the_command() {
        for line in [
            r#"Run \"snob followers {}\" once and the monitor will have @someone stored."#,
            "Run `snob scan someone` to see @someone's picture.",
            "A username may be written with or without a leading @.",
            r#"assert!(text.contains("for details, run \"snob fans someone\""));"#,
            "  snob unfollowers                    who does not follow you back",
        ] {
            assert!(
                at_name_in_a_command(line).is_none(),
                "false positive on: {line}"
            );
        }
    }

    #[test]
    fn the_escape_hatch_is_honored_by_the_caller() {
        // The line filter lives in the walker; here we only check the detector
        // would otherwise have fired.
        assert!(spanish_in("valor = 'usuario' // i18n-allow").is_some());
    }
}
