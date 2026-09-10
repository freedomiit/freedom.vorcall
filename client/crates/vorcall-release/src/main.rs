//! The release-side half of the signed updater: it makes the signing key,
//! assembles the manifest from the built binaries, signs it, and checks the
//! result the same way a client will. Driven by `.github/workflows/release.yml`.

mod args;
mod keys;
mod manifest;

use anyhow::Result;

use args::{Command, Parsed};

const USAGE: &str = "\
vorcall-release <subcommand> [options]

Subcommands:
  gen-key --out <file>
      Generate an Ed25519 key pair. The private key goes to <file> (which must
      not exist yet); the public key is printed as 64 hexadecimal characters.

  manifest --version <v> --min-version <v> [--published-at <rfc3339>]
           [--notes-file <file>] --asset <platform>=<path> [--asset ...]
           --out <manifest.json>
      Build the release manifest. Platforms: linux-x86_64, windows-x86_64,
      macos-aarch64. --published-at defaults to now, --notes-file to no notes.

  sign (--key-env <var> | --key-file <file>) <manifest.json> --out <manifest.sig>
      Sign the exact bytes of <manifest.json> with the private key.

  verify (--keys <keyfile> | --key <hex>) <manifest.json> <manifest.sig>
      Check the signature the way a client does. Prints \"ok <version>\".

  --help  print this help

Exit codes: 0 ok, 1 failure, 2 usage.";

fn main() {
    std::process::exit(run(std::env::args().skip(1).collect()));
}

fn run(argv: Vec<String>) -> i32 {
    match args::parse(&argv) {
        Ok(Parsed::Help) => {
            println!("{USAGE}");
            0
        }
        Ok(Parsed::Run(command)) => match execute(command) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("vorcall-release: {e:#}");
                1
            }
        },
        Err(e) => {
            eprintln!("vorcall-release: {e}\n\n{USAGE}");
            2
        }
    }
}

fn execute(command: Command) -> Result<()> {
    match command {
        Command::GenKey { out } => {
            let public = keys::generate(&out)?;
            println!("{public}");
        }
        Command::Manifest(spec) => {
            let built = manifest::build(&spec)?;
            manifest::write(&built, &spec.out)?;
        }
        Command::Sign { key, manifest, out } => {
            let pair = keys::load_pair(&key)?;
            keys::sign_file(&pair, &manifest, &out)?;
        }
        Command::Verify {
            keys: source,
            manifest: path,
            signature,
        } => {
            let trusted = keys::load_public(&source)?;
            let version = manifest::verify(&path, &signature, &trusted)?;
            println!("ok {version}");
        }
    }

    Ok(())
}

#[cfg(test)]
mod testing {
    //! A temporary directory of this test's own, removed when it passes.

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    pub fn temp_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "vorcall-release-{label}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("should create the test directory");
        dir
    }

    pub fn remove_dir(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn help_exits_zero_and_an_unknown_subcommand_exits_two() {
        assert_eq!(run(argv(&["--help"])), 0);
        assert_eq!(run(argv(&[])), 2);
        assert_eq!(run(argv(&["publish"])), 2);
    }

    #[test]
    fn the_usage_text_lists_every_subcommand() {
        for subcommand in ["gen-key", "manifest", "sign", "verify"] {
            assert!(USAGE.contains(subcommand), "{subcommand} is missing");
        }
    }

    #[test]
    fn the_full_pipeline_exits_zero() {
        let dir = testing::temp_dir("pipeline");
        let binary = dir.join("vorcall-linux-x86_64");
        std::fs::write(&binary, b"payload").expect("should write");
        let key = dir.join("release.key");
        let manifest_path = dir.join("manifest.json");
        let signature = dir.join("manifest.sig");
        let public_keys = dir.join("update-keys.pub");

        assert_eq!(
            run(argv(&["gen-key", "--out", key.to_str().expect("utf-8")])),
            0
        );
        // gen-key prints the public key; the test reads it back through the
        // library rather than capturing stdout.
        let public = keys::generate(&dir.join("second.key")).expect("should generate");
        std::fs::write(&public_keys, format!("# keys\n{public}\n")).expect("should write");

        assert_eq!(
            run(argv(&[
                "manifest",
                "--version",
                "0.3.0",
                "--min-version",
                "0.2.0",
                "--asset",
                &format!("linux-x86_64={}", binary.display()),
                "--out",
                manifest_path.to_str().expect("utf-8"),
            ])),
            0
        );
        assert_eq!(
            run(argv(&[
                "sign",
                "--key-file",
                dir.join("second.key").to_str().expect("utf-8"),
                manifest_path.to_str().expect("utf-8"),
                "--out",
                signature.to_str().expect("utf-8"),
            ])),
            0
        );
        assert_eq!(
            run(argv(&[
                "verify",
                "--keys",
                public_keys.to_str().expect("utf-8"),
                manifest_path.to_str().expect("utf-8"),
                signature.to_str().expect("utf-8"),
            ])),
            0
        );

        // The key that did not sign it must not verify it.
        let other = keys::generate(&dir.join("third.key")).expect("should generate");
        assert_eq!(
            run(argv(&[
                "verify",
                "--key",
                &other,
                manifest_path.to_str().expect("utf-8"),
                signature.to_str().expect("utf-8"),
            ])),
            1
        );

        testing::remove_dir(&dir);
    }
}
