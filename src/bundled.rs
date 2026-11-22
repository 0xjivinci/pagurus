//! Skills that ship inside the binary and are installed into
//! $PAGURUS_HOME/skills on `init` / `setup` / `skills install` — only where no
//! skill of that name exists, so user and learned edits are never clobbered.

use std::path::Path;

pub const SKILLS: &[(&str, &str)] = &[
    ("pi-health", include_str!("../skills-bundled/pi-health/SKILL.md")),
    ("service-check", include_str!("../skills-bundled/service-check/SKILL.md")),
    ("disk-cleanup", include_str!("../skills-bundled/disk-cleanup/SKILL.md")),
    ("log-triage", include_str!("../skills-bundled/log-triage/SKILL.md")),
    ("setup-alert", include_str!("../skills-bundled/setup-alert/SKILL.md")),
    ("network-check", include_str!("../skills-bundled/network-check/SKILL.md")),
    ("write-a-skill", include_str!("../skills-bundled/write-a-skill/SKILL.md")),
    ("screenshot", include_str!("../skills-bundled/screenshot/SKILL.md")),
    ("research-report", include_str!("../skills-bundled/research-report/SKILL.md")),
    ("plan-a-project", include_str!("../skills-bundled/plan-a-project/SKILL.md")),
    ("summarize", include_str!("../skills-bundled/summarize/SKILL.md")),
];

/// Skills that need an optional feature (the uge build).
#[cfg(feature = "art")]
pub const EXTRA: &[(&str, &str)] = &[
    ("art-collector", include_str!("../skills-bundled/art-collector/SKILL.md")),
    ("art-artist", include_str!("../skills-bundled/art-artist/SKILL.md")),
    ("art-taste", include_str!("../skills-bundled/art-taste/SKILL.md")),
    ("find-a-skill", include_str!("../skills-bundled/find-a-skill/SKILL.md")),
    ("coding", include_str!("../skills-bundled/coding/SKILL.md")),
    ("debugging", include_str!("../skills-bundled/debugging/SKILL.md")),
    ("git-workflow", include_str!("../skills-bundled/git-workflow/SKILL.md")),
    ("make-a-document", include_str!("../skills-bundled/make-a-document/SKILL.md")),
];
#[cfg(not(feature = "art"))]
pub const EXTRA: &[(&str, &str)] = &[];

/// Bundled skills this home was already offered: a new pagurus installs only the
/// skills that are new since then, so a skill the user deleted stays deleted.
const OFFERED: &str = ".bundled";

/// At startup: install bundled skills this home has never been offered.
/// Returns the names installed.
pub fn install_new(home: &Path) -> Vec<String> {
    let marker = home.join("skills").join(OFFERED);
    let offered = crate::util::read(&marker);
    let offered: Vec<&str> = offered.lines().collect();
    let mut installed = Vec::new();
    for (name, body) in SKILLS.iter().chain(EXTRA) {
        if offered.contains(name) {
            continue;
        }
        let p = home.join("skills").join(name).join("SKILL.md");
        if !p.exists() && !crate::skills::is_archived(home, name) && crate::util::write_atomic(&p, body).is_ok() {
            installed.push(name.to_string());
        }
    }
    let all: Vec<&str> = SKILLS.iter().chain(EXTRA).map(|(n, _)| *n).chain(offered.iter().copied()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    if all.len() != offered.len() {
        let _ = crate::util::write_atomic(&marker, &(all.join("\n") + "\n"));
    }
    installed
}

/// Returns (installed, kept) names.
pub fn install(home: &Path) -> Result<(Vec<String>, Vec<String>), String> {
    let (mut installed, mut kept) = (Vec::new(), Vec::new());
    for (name, body) in SKILLS.iter().chain(EXTRA) {
        let p = home.join("skills").join(name).join("SKILL.md");
        if p.exists() || crate::skills::is_archived(home, name) {
            kept.push(name.to_string());
        } else {
            crate::util::write_atomic(&p, body)?;
            installed.push(name.to_string());
        }
    }
    Ok((installed, kept))
}

#[cfg(test)]
mod tests {
    #[test]
    fn bundled_skills_are_well_formed_and_small() {
        for (name, body) in super::SKILLS.iter().chain(super::EXTRA) {
            crate::learn::valid_name(name).unwrap();
            assert!(body.starts_with(&format!("---\nname: {name}\ndescription: ")), "{name} frontmatter");
            assert!(body.len() < 3000, "{name} is {} bytes; keep skills short", body.len());
        }
        let home = std::env::temp_dir().join(format!("pagurus-bundled-{}", crate::util::new_id()));
        let all = super::SKILLS.len() + super::EXTRA.len();
        let (inst, kept) = super::install(&home).unwrap();
        assert_eq!((inst.len(), kept.len()), (all, 0));
        std::fs::write(home.join("skills/pi-health/SKILL.md"), "mine").unwrap();
        let (inst, kept) = super::install(&home).unwrap();
        assert_eq!((inst.len(), kept.len()), (0, all));
        assert_eq!(std::fs::read_to_string(home.join("skills/pi-health/SKILL.md")).unwrap(), "mine");
        assert_eq!(crate::tools::skill_index(&home).len(), all);
    }
    #[test]
    fn new_bundled_skills_arrive_once_and_deleted_ones_stay_deleted() {
        let home = std::env::temp_dir().join(format!("pagurus-bundled-new-{}", crate::util::new_id()));
        let all = super::SKILLS.len() + super::EXTRA.len();
        assert_eq!(super::install_new(&home).len(), all, "a home that never had them gets all");
        std::fs::remove_dir_all(home.join("skills/summarize")).unwrap();
        assert!(super::install_new(&home).is_empty(), "deleted stays deleted");
        // a home from before this pack: it lists only the old skills as offered
        std::fs::write(home.join("skills/.bundled"), "pi-health\n").unwrap();
        assert_eq!(super::install_new(&home), vec!["summarize".to_string()], "only what's missing and new to it");
        let _ = std::fs::remove_dir_all(&home);
    }
}
