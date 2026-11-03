//! The targets a release builds, the formula it publishes, and what the docs
//! promise must agree: a missing archive breaks `brew install` only after the
//! release, when nothing can fix it.

use std::process::Command;

const RELEASE: &str = include_str!("../.github/workflows/release.yml");
const FORMULA_SCRIPT: &str = include_str!("../.github/scripts/homebrew-formula.sh");
const INSTALL: &str = include_str!("../docs/user/install.md");

fn release_targets() -> Vec<&'static str> {
    RELEASE.lines().filter_map(|l| l.trim().strip_prefix("- target: ")).collect()
}

fn formula(digests: &[(&str, &str)]) -> std::process::Output {
    let dir = std::env::temp_dir().join(format!("xolotl-formula-{}-{}", std::process::id(), digests.len()));
    std::fs::create_dir_all(&dir).unwrap();
    for (target, digest) in digests {
        std::fs::write(dir.join(format!("xolotl-1.2.3-{target}.tar.gz.sha256")), format!("{digest}  xolotl-1.2.3-{target}.tar.gz\n"))
            .unwrap();
    }
    let output = Command::new("bash").arg(".github/scripts/homebrew-formula.sh").arg("1.2.3").arg(&dir).output().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    output
}

#[test]
fn release_builds_every_target_the_formula_and_docs_name() {
    let targets = release_targets();
    assert_eq!(targets, ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "aarch64-apple-darwin"]);

    let mut in_script: Vec<&str> =
        FORMULA_SCRIPT.lines().filter_map(|l| l.split_once("$(digest ")).filter_map(|(_, r)| r.strip_suffix(')')).collect();
    let mut sorted = targets.clone();
    sorted.sort();
    in_script.sort();
    assert_eq!(in_script, sorted);

    for target in targets {
        assert!(INSTALL.contains(&format!("| `{target}` |")), "{target} is not in the install docs");
    }
}

#[test]
fn the_formula_names_each_archive_with_its_digest() {
    let digests: Vec<(&str, String)> = release_targets().into_iter().enumerate().map(|(i, t)| (t, format!("{i}").repeat(64))).collect();
    let refs: Vec<(&str, &str)> = digests.iter().map(|(t, d)| (*t, d.as_str())).collect();
    let output = formula(&refs);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let text = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    for (target, digest) in &refs {
        let url = format!("url \"https://github.com/wyrd-company/xolotl/releases/download/1.2.3/xolotl-1.2.3-{target}.tar.gz\"");
        let at = lines.iter().position(|l| *l == url).unwrap_or_else(|| panic!("{url}\n{text}"));
        assert_eq!(lines[at + 1], format!("sha256 \"{digest}\""), "{target}");
    }
    assert!(lines.contains(&"version \"1.2.3\""));
}

#[test]
fn the_formula_fails_without_every_digest() {
    let digest = "a".repeat(64);
    let all = release_targets();
    let refs: Vec<(&str, &str)> = all.iter().map(|t| (*t, digest.as_str())).collect();
    for missing in 0..refs.len() {
        let mut some = refs.clone();
        some.remove(missing);
        assert!(!formula(&some).status.success(), "no digest for {}", refs[missing].0);
    }
    let mut empty = refs.clone();
    empty[0].1 = "";
    assert!(!formula(&empty).status.success(), "an empty digest");
}
