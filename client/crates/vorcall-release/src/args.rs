//! Hand-rolled argv parsing, in the shape the probe already uses: the
//! subcommand is `argv[1]` and everything after it is flags.

use std::path::PathBuf;

use vorcall_core::update::Version;

/// The platform ids a release may carry; anything else is a usage error, so a
/// typo in the workflow never ships as an entry no client looks for.
pub const PLATFORMS: [&str; 3] = ["linux-x86_64", "windows-x86_64", "macos-aarch64"];

pub enum Parsed {
    Help,
    Run(Command),
}

pub enum Command {
    GenKey {
        out: PathBuf,
    },
    Manifest(ManifestSpec),
    Sign {
        key: KeySource,
        manifest: PathBuf,
        out: PathBuf,
    },
    Verify {
        keys: KeySource,
        manifest: PathBuf,
        signature: PathBuf,
    },
}

pub struct ManifestSpec {
    pub version: Version,
    pub min_version: Version,
    pub published_at: Option<String>,
    pub notes_file: Option<PathBuf>,
    pub assets: Vec<(String, PathBuf)>,
    pub out: PathBuf,
}

pub enum KeySource {
    Env(String),
    File(PathBuf),
    Hex(String),
}

/// `Err` is always a usage error (exit 2).
pub fn parse(args: &[String]) -> Result<Parsed, String> {
    let Some(subcommand) = args.first() else {
        return Err("a subcommand is required".to_owned());
    };
    let rest = &args[1..];

    match subcommand.as_str() {
        "--help" | "-h" | "help" => Ok(Parsed::Help),
        "gen-key" => gen_key(rest),
        "manifest" => manifest(rest),
        "sign" => sign(rest),
        "verify" => verify(rest),
        other => Err(format!("unknown subcommand {other}")),
    }
}

fn gen_key(args: &[String]) -> Result<Parsed, String> {
    let mut out = None;

    let mut args = args.iter();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--help" | "-h" => return Ok(Parsed::Help),
            "--out" => set_once(&mut out, flag, value(flag, &mut args)?)?,
            other => return Err(unknown(other)),
        }
    }

    Ok(Parsed::Run(Command::GenKey {
        out: PathBuf::from(required(out, "--out")?),
    }))
}

fn manifest(args: &[String]) -> Result<Parsed, String> {
    let mut version = None;
    let mut min_version = None;
    let mut published_at = None;
    let mut notes_file = None;
    let mut out = None;
    let mut assets: Vec<(String, PathBuf)> = Vec::new();

    let mut args = args.iter();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--help" | "-h" => return Ok(Parsed::Help),
            "--version" => set_once(&mut version, flag, value(flag, &mut args)?)?,
            "--min-version" => set_once(&mut min_version, flag, value(flag, &mut args)?)?,
            "--published-at" => set_once(&mut published_at, flag, value(flag, &mut args)?)?,
            "--notes-file" => set_once(&mut notes_file, flag, value(flag, &mut args)?)?,
            "--out" => set_once(&mut out, flag, value(flag, &mut args)?)?,
            "--asset" => {
                let (platform, path) = asset(&value(flag, &mut args)?)?;
                if assets.iter().any(|(known, _)| known == &platform) {
                    return Err(format!("--asset {platform} was given twice"));
                }
                assets.push((platform, path));
            }
            other => return Err(unknown(other)),
        }
    }

    if assets.is_empty() {
        return Err("at least one --asset is required".to_owned());
    }
    let published_at = match published_at {
        Some(value) if value.trim().is_empty() => {
            return Err("--published-at cannot be empty".to_owned());
        }
        other => other,
    };

    Ok(Parsed::Run(Command::Manifest(ManifestSpec {
        version: version_flag(required(version, "--version")?, "--version")?,
        min_version: version_flag(required(min_version, "--min-version")?, "--min-version")?,
        published_at,
        notes_file: notes_file.map(PathBuf::from),
        assets,
        out: PathBuf::from(required(out, "--out")?),
    })))
}

fn sign(args: &[String]) -> Result<Parsed, String> {
    let mut key_env = None;
    let mut key_file = None;
    let mut out = None;
    let mut manifest = None;

    let mut args = args.iter();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--help" | "-h" => return Ok(Parsed::Help),
            "--key-env" => set_once(&mut key_env, flag, value(flag, &mut args)?)?,
            "--key-file" => set_once(&mut key_file, flag, value(flag, &mut args)?)?,
            "--out" => set_once(&mut out, flag, value(flag, &mut args)?)?,
            other if other.starts_with('-') => return Err(unknown(other)),
            positional => set_once(&mut manifest, "<manifest.json>", positional.to_owned())?,
        }
    }

    let key = match (key_env, key_file) {
        (Some(variable), None) => KeySource::Env(variable),
        (None, Some(path)) => KeySource::File(PathBuf::from(path)),
        _ => return Err("exactly one of --key-env or --key-file is required".to_owned()),
    };

    Ok(Parsed::Run(Command::Sign {
        key,
        manifest: PathBuf::from(required(manifest, "<manifest.json>")?),
        out: PathBuf::from(required(out, "--out")?),
    }))
}

fn verify(args: &[String]) -> Result<Parsed, String> {
    let mut key_file = None;
    let mut key_hex = None;
    let mut positional: Vec<String> = Vec::new();

    let mut args = args.iter();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--help" | "-h" => return Ok(Parsed::Help),
            "--keys" => set_once(&mut key_file, flag, value(flag, &mut args)?)?,
            "--key" => set_once(&mut key_hex, flag, value(flag, &mut args)?)?,
            other if other.starts_with('-') => return Err(unknown(other)),
            other => positional.push(other.to_owned()),
        }
    }

    let keys = match (key_file, key_hex) {
        (Some(path), None) => KeySource::File(PathBuf::from(path)),
        (None, Some(hex)) => KeySource::Hex(hex),
        _ => return Err("exactly one of --keys or --key is required".to_owned()),
    };
    let [manifest, signature] = positional.as_slice() else {
        return Err("verify wants <manifest.json> and <manifest.sig>".to_owned());
    };

    Ok(Parsed::Run(Command::Verify {
        keys,
        manifest: PathBuf::from(manifest),
        signature: PathBuf::from(signature),
    }))
}

/// Split on the FIRST `=`: a Windows path may carry one of its own.
fn asset(raw: &str) -> Result<(String, PathBuf), String> {
    let (platform, path) = raw
        .split_once('=')
        .ok_or_else(|| format!("--asset wants <platform>=<path>, got {raw}"))?;

    if !PLATFORMS.contains(&platform) {
        return Err(format!(
            "unknown platform {platform}; known: {}",
            PLATFORMS.join(", ")
        ));
    }
    if path.is_empty() {
        return Err(format!("--asset {platform} has no path"));
    }

    Ok((platform.to_owned(), PathBuf::from(path)))
}

fn version_flag(raw: String, flag: &str) -> Result<Version, String> {
    raw.parse()
        .map_err(|_| format!("{flag} wants a major.minor.patch version, got {raw}"))
}

fn value<'a>(flag: &str, args: &mut impl Iterator<Item = &'a String>) -> Result<String, String> {
    args.next()
        .cloned()
        .ok_or_else(|| format!("{flag} wants a value"))
}

fn set_once(slot: &mut Option<String>, flag: &str, value: String) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("{flag} was given twice"));
    }
    *slot = Some(value);
    Ok(())
}

fn required(slot: Option<String>, flag: &str) -> Result<String, String> {
    slot.ok_or_else(|| format!("{flag} is required"))
}

fn unknown(flag: &str) -> String {
    format!("unknown argument {flag}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|value| (*value).to_owned()).collect()
    }

    fn parse_raw(raw: &[&str]) -> Result<Parsed, String> {
        parse(&args(raw))
    }

    #[test]
    fn help_is_recognised() {
        assert!(matches!(parse_raw(&["--help"]), Ok(Parsed::Help)));
        assert!(matches!(parse_raw(&["-h"]), Ok(Parsed::Help)));
        assert!(matches!(
            parse_raw(&["gen-key", "--help"]),
            Ok(Parsed::Help)
        ));
    }

    #[test]
    fn no_arguments_is_a_usage_error() {
        assert!(parse_raw(&[]).is_err());
    }

    #[test]
    fn an_unknown_subcommand_is_a_usage_error() {
        assert!(parse_raw(&["publish"]).is_err());
    }

    #[test]
    fn an_unknown_platform_is_a_usage_error() {
        let error = parse_raw(&[
            "manifest",
            "--version",
            "0.2.0",
            "--min-version",
            "0.2.0",
            "--asset",
            "linux-aarch64=/tmp/vorcall",
            "--out",
            "/tmp/m.json",
        ])
        .err()
        .expect("should be a usage error");

        assert!(error.contains("unknown platform"), "{error}");
    }

    #[test]
    fn a_duplicate_platform_is_a_usage_error() {
        assert!(
            parse_raw(&[
                "manifest",
                "--version",
                "0.2.0",
                "--min-version",
                "0.2.0",
                "--asset",
                "linux-x86_64=/tmp/a",
                "--asset",
                "linux-x86_64=/tmp/b",
                "--out",
                "/tmp/m.json",
            ])
            .is_err()
        );
    }

    #[test]
    fn a_missing_asset_is_a_usage_error() {
        assert!(
            parse_raw(&[
                "manifest",
                "--version",
                "0.2.0",
                "--min-version",
                "0.2.0",
                "--out",
                "/tmp/m.json",
            ])
            .is_err()
        );
    }

    #[test]
    fn an_unparseable_version_is_a_usage_error() {
        assert!(
            parse_raw(&[
                "manifest",
                "--version",
                "v0.2",
                "--min-version",
                "0.2.0",
                "--asset",
                "linux-x86_64=/tmp/a",
                "--out",
                "/tmp/m.json",
            ])
            .is_err()
        );
    }

    #[test]
    fn the_asset_value_splits_on_the_first_equals() {
        let (platform, path) = asset("windows-x86_64=C:=dir\\vorcall.exe").expect("should split");

        assert_eq!(platform, "windows-x86_64");
        assert_eq!(path, PathBuf::from("C:=dir\\vorcall.exe"));
    }

    #[test]
    fn sign_wants_exactly_one_key_source() {
        assert!(
            parse_raw(&[
                "sign",
                "--key-env",
                "KEY",
                "--key-file",
                "/tmp/key",
                "m.json",
                "--out",
                "m.sig",
            ])
            .is_err()
        );
        assert!(parse_raw(&["sign", "m.json", "--out", "m.sig"]).is_err());
        assert!(matches!(
            parse_raw(&["sign", "--key-env", "KEY", "m.json", "--out", "m.sig"]),
            Ok(Parsed::Run(Command::Sign { .. }))
        ));
    }

    #[test]
    fn verify_wants_two_positional_paths() {
        assert!(parse_raw(&["verify", "--key", "ab", "m.json"]).is_err());
        assert!(matches!(
            parse_raw(&["verify", "--key", "ab", "m.json", "m.sig"]),
            Ok(Parsed::Run(Command::Verify { .. }))
        ));
    }

    #[test]
    fn a_repeated_flag_is_a_usage_error() {
        assert!(parse_raw(&["gen-key", "--out", "a", "--out", "b"]).is_err());
    }
}
