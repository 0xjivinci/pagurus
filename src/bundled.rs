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
];

/// Skills that need an optional feature (the uge build).
#[cfg(feature = "art")]
pub const EXTRA: &[(&str, &str)] = &[
    ("art-collector", include_str!("../skills-bundled/art-collector/SKILL.md")),
    ("art-artist", include_str!("../skills-bundled/art-artist/SKILL.md")),
    ("art-taste", include_str!("../skills-bundled/art-taste/SKILL.md")),
];
#[cfg(not(feature = "art"))]
pub const EXTRA: &[(&str, &str)] = &[];

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
}
