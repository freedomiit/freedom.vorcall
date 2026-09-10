//! The two update subcommands: `check-update` walks the real update path up to
//! a verified download, `apply-update` performs the swap on the probe's own
//! binary. Together with the relaunch marker in `main` they are the runtime
//! oracle for the updater.

use std::ffi::OsString;
use std::path::Path;

use serde_json::json;
use vorcall_core::update::{self, UpdateError, Version};

/// How a run ended. Refusals (exit 1) are the server or the payload saying no
/// and carry a stage, so an oracle can assert on which check rejected it;
/// operational failures (exit 2) are this probe or the network not getting far
/// enough to judge.
enum Failure {
    /// Printed on stderr, exit 2.
    Operational(String),
    /// Printed on stdout as JSON, exit 1.
    Refusal { error: String, stage: &'static str },
}

struct CheckArgs {
    username: String,
    password: String,
    platform: String,
    out: Option<String>,
    pubkeys: Vec<String>,
    no_download: bool,
}

/// `args` is the whole command line after the program name, the subcommand
/// included.
pub async fn check_update(args: Vec<String>) -> i32 {
    let parsed = match parse_check(&args) {
        Ok(parsed) => parsed,
        Err(reason) => {
            eprintln!("vorcall-probe: {reason}");
            return 2;
        }
    };

    match run_check(parsed).await {
        Ok(report) => {
            println!("{report}");
            0
        }
        Err(Failure::Operational(reason)) => {
            eprintln!("vorcall-probe: {reason}");
            2
        }
        Err(Failure::Refusal { error, stage }) => {
            println!("{}", json!({ "error": error, "stage": stage }));
            1
        }
    }
}

async fn run_check(args: CheckArgs) -> Result<String, Failure> {
    let keys = if args.pubkeys.is_empty() {
        update::keys::baked().map_err(|e| Failure::Refusal {
            error: e.to_string(),
            stage: "manifest",
        })?
    } else {
        update::keys::parse(&args.pubkeys.join("\n")).map_err(|e| {
            // A key handed in on the command line is a usage mistake, not the
            // server refusing anything.
            Failure::Operational(e.to_string())
        })?
    };

    let endpoints =
        vorcall_core::endpoints::resolve().map_err(|e| Failure::Operational(format!("{e:#}")))?;
    tracing::info!(?endpoints, "resolved endpoints");

    let session = vorcall_core::auth::login(&endpoints, &args.username, &args.password)
        .await
        .map_err(|e| Failure::Operational(format!("sign-in failed: {e}")))?;

    let (bytes, signature) = update::client::fetch_manifest(&endpoints, &session.access_token)
        .await
        .map_err(refusal_or_operational)?;
    let manifest = update::manifest::verify_and_parse(&bytes, &signature, &keys)
        .map_err(refusal_or_operational)?;

    let current = Version::current();
    let asset = manifest.platforms.get(&args.platform).cloned();

    let mut downloaded = None;
    let mut verified = false;
    if let Some(asset) = asset.as_ref()
        && !args.no_download
    {
        let out = args.out.as_ref().ok_or_else(|| {
            Failure::Operational("--out is required without --no-download".into())
        })?;
        let path = Path::new(out);

        // `download_asset` opens `dest` without creating it.
        let result = async {
            std::fs::File::create(path).map_err(UpdateError::Io)?;
            update::client::download_asset(
                &endpoints,
                &session.access_token,
                &manifest.version,
                asset,
                path,
                |_, _| {},
            )
            .await
        }
        .await;
        if let Err(e) = result {
            let _ = std::fs::remove_file(path);
            return Err(download_refusal(e));
        }

        downloaded = Some(out.clone());
        verified = true;
    }

    Ok(json!({
        "current_version": current.to_string(),
        "manifest_version": manifest.version.to_string(),
        "min_version": manifest.min_version.to_string(),
        "update_available": manifest.version > current,
        "required": manifest.min_version > current,
        "platform": args.platform,
        "asset": asset.as_ref().map(|asset| asset.path.clone()),
        "size": asset.as_ref().map(|asset| asset.size),
        "sha256": asset.as_ref().map(|asset| asset.sha256.clone()),
        "downloaded": downloaded,
        "verified": verified,
    })
    .to_string())
}

/// The stages of the exchange the payload can be refused at. Anything the
/// transport itself failed at is operational instead.
fn refusal_or_operational(error: UpdateError) -> Failure {
    let stage = match error {
        UpdateError::Signature => "signature",
        UpdateError::Manifest(_) | UpdateError::Version(_) | UpdateError::Keys(_) => "manifest",
        UpdateError::Size { .. } => "size",
        UpdateError::Hash { .. } => "hash",
        UpdateError::Io(_) => "download",
        UpdateError::Api(_) | UpdateError::Disabled(_) | UpdateError::Swap(_) => {
            return Failure::Operational(error.to_string());
        }
    };
    Failure::Refusal {
        error: error.to_string(),
        stage,
    }
}

/// Inside the download every failure is a refusal of the payload, transport
/// errors included: the manifest promised a file that did not arrive intact.
fn download_refusal(error: UpdateError) -> Failure {
    let stage = match error {
        UpdateError::Size { .. } => "size",
        UpdateError::Hash { .. } => "hash",
        _ => "download",
    };
    Failure::Refusal {
        error: error.to_string(),
        stage,
    }
}

pub fn apply_update(args: Vec<String>) -> i32 {
    let file = match parse_apply(&args) {
        Ok(file) => file,
        Err(reason) => {
            eprintln!("vorcall-probe: {reason}");
            return 2;
        }
    };

    let relaunch: Vec<OsString> = args.iter().map(OsString::from).collect();
    match update::swap::apply_and_relaunch(Path::new(&file), &relaunch) {
        // Unix never gets here on success: the process image is replaced.
        Ok(update::swap::Relaunched::Spawned) => {
            println!("{}", json!({ "spawned": true }));
            0
        }
        Err(e) => {
            println!("{}", json!({ "error": e.to_string(), "stage": "swap" }));
            1
        }
    }
}

fn parse_check(args: &[String]) -> Result<CheckArgs, String> {
    let mut username = None;
    let mut password = None;
    let mut platform = None;
    let mut out = None;
    let mut pubkeys = Vec::new();
    let mut no_download = false;

    let mut args = args.iter().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--no-download" => no_download = true,
            "--username" => username = Some(value(flag, &mut args)?),
            "--password" => password = Some(value(flag, &mut args)?),
            "--platform" => platform = Some(value(flag, &mut args)?),
            "--out" => out = Some(value(flag, &mut args)?),
            "--pubkey" => pubkeys.push(value(flag, &mut args)?),
            other => return Err(format!("unknown argument {other}")),
        }
    }

    let username = username.ok_or("--username is required")?;
    let password = password
        .or_else(|| std::env::var("VORCALL_PROBE_PASSWORD").ok())
        .filter(|password| !password.is_empty())
        .ok_or("--password or VORCALL_PROBE_PASSWORD is required")?;
    let platform = platform.ok_or("--platform is required")?;
    if out.is_none() && !no_download {
        return Err("--out is required without --no-download".to_owned());
    }

    Ok(CheckArgs {
        username,
        password,
        platform,
        out,
        pubkeys,
        no_download,
    })
}

fn parse_apply(args: &[String]) -> Result<String, String> {
    let mut file = None;

    let mut args = args.iter().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--file" => file = Some(value(flag, &mut args)?),
            other => return Err(format!("unknown argument {other}")),
        }
    }

    file.ok_or_else(|| "--file is required".to_owned())
}

fn value<'a>(flag: &str, args: &mut impl Iterator<Item = &'a String>) -> Result<String, String> {
    args.next()
        .cloned()
        .ok_or_else(|| format!("{flag} wants a value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|arg| (*arg).to_string()).collect()
    }

    #[test]
    fn check_takes_every_flag() {
        let parsed = parse_check(&args(&[
            "check-update",
            "--username",
            "alice",
            "--password",
            "secret",
            "--platform",
            "linux-x86_64",
            "--out",
            "pending.bin",
            "--pubkey",
            "aa",
            "--pubkey",
            "bb",
        ]))
        .expect("should parse");

        assert_eq!(parsed.username, "alice");
        assert_eq!(parsed.password, "secret");
        assert_eq!(parsed.platform, "linux-x86_64");
        assert_eq!(parsed.out.as_deref(), Some("pending.bin"));
        assert_eq!(parsed.pubkeys, vec!["aa".to_owned(), "bb".to_owned()]);
        assert!(!parsed.no_download);
    }

    #[test]
    fn check_wants_an_out_path_unless_downloading_is_off() {
        let without = args(&[
            "check-update",
            "--username",
            "alice",
            "--password",
            "secret",
            "--platform",
            "linux-x86_64",
        ]);
        assert!(parse_check(&without).is_err());

        let mut skipped = without.clone();
        skipped.push("--no-download".to_owned());
        let parsed = parse_check(&skipped).expect("should parse");

        assert!(parsed.no_download);
        assert!(parsed.out.is_none());
    }

    #[test]
    fn check_rejects_an_unknown_flag() {
        let parsed = parse_check(&args(&[
            "check-update",
            "--username",
            "alice",
            "--password",
            "secret",
            "--platform",
            "linux-x86_64",
            "--no-download",
            "--tone-hz",
        ]));

        assert_eq!(parsed.err().as_deref(), Some("unknown argument --tone-hz"));
    }

    #[test]
    fn check_wants_a_value_after_a_flag() {
        let parsed = parse_check(&args(&["check-update", "--username"]));

        assert_eq!(parsed.err().as_deref(), Some("--username wants a value"));
    }

    #[test]
    fn apply_takes_the_file() {
        let file =
            parse_apply(&args(&["apply-update", "--file", "/tmp/new"])).expect("should parse");

        assert_eq!(file, "/tmp/new");
    }

    #[test]
    fn apply_requires_the_file() {
        assert_eq!(
            parse_apply(&args(&["apply-update"])).err().as_deref(),
            Some("--file is required")
        );
    }
}
