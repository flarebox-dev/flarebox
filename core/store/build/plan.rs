// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, PartialEq, Eq)]
pub struct Planned {
    pub name: String,
    pub requires: Vec<String>,
}

pub fn plan(legacy: &[&str], named: &[(&str, &str)]) -> Result<Vec<Planned>, String> {
    for (index, name) in legacy.iter().enumerate() {
        let prefix = format!("{:02}_", index + 1);
        if !name.starts_with(&prefix) {
            return Err(format!(
                "legacy migration {name} is step {}, so its name starts with {prefix}",
                index + 1
            ));
        }
    }
    let mut requires = BTreeMap::new();
    for &(name, sql) in named {
        if !name.starts_with(|c: char| c.is_ascii_lowercase())
            || !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(format!(
                "migration {name} needs a name of lowercase letters, digits and underscores that starts with a letter"
            ));
        }
        requires.insert(name, required(sql));
    }
    for (name, required) in &requires {
        if let Some(missing) = required.iter().find(|r| !requires.contains_key(r.as_str())) {
            return Err(format!(
                "migration {name} requires {missing}, which is not a named migration in core/store/migrations"
            ));
        }
    }
    let mut planned: Vec<Planned> = legacy
        .iter()
        .map(|name| Planned {
            name: (*name).to_owned(),
            requires: vec![],
        })
        .collect();
    let mut placed = BTreeSet::new();
    while placed.len() < requires.len() {
        let Some((&name, required)) = requires.iter().find(|(name, required)| {
            !placed.contains(*name) && required.iter().all(|r| placed.contains(r.as_str()))
        }) else {
            let cycle: Vec<&str> = requires
                .keys()
                .copied()
                .filter(|name| !placed.contains(name))
                .collect();
            return Err(format!(
                "migrations {} require each other in a cycle",
                cycle.join(", ")
            ));
        };
        placed.insert(name);
        planned.push(Planned {
            name: name.to_owned(),
            requires: required.clone(),
        });
    }
    Ok(planned)
}

fn required(sql: &str) -> Vec<String> {
    sql.lines()
        .next()
        .and_then(|line| line.strip_prefix("-- requires:"))
        .map(|list| {
            list.split(',')
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(planned: &[Planned]) -> Vec<&str> {
        planned.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn legacy_steps_come_first_in_their_order_then_named_ones_by_name() {
        let planned = plan(
            &["01_initial", "02_link"],
            &[
                ("zeta", "CREATE TABLE z (a);"),
                ("alpha", "CREATE TABLE a (a);"),
            ],
        )
        .unwrap();
        assert_eq!(names(&planned), ["01_initial", "02_link", "alpha", "zeta"]);
    }

    #[test]
    fn a_migration_comes_after_the_ones_it_requires() {
        let planned = plan(
            &["01_initial"],
            &[
                (
                    "alpha",
                    "-- requires: zeta, mid\nALTER TABLE z ADD COLUMN b;",
                ),
                ("mid", "-- requires: zeta\nCREATE TABLE m (a);"),
                ("zeta", "CREATE TABLE z (a);"),
            ],
        )
        .unwrap();
        assert_eq!(names(&planned), ["01_initial", "zeta", "mid", "alpha"]);
        assert_eq!(planned[3].requires, ["zeta", "mid"]);
    }

    #[test]
    fn a_requirement_that_names_no_migration_is_refused() {
        let error = plan(&[], &[("alpha", "-- requires: 01_initial\nSELECT 1;")]).unwrap_err();
        assert!(error.contains("requires 01_initial"), "{error}");
        let error = plan(&[], &[("alpha", "-- requires: gone\nSELECT 1;")]).unwrap_err();
        assert!(error.contains("requires gone"), "{error}");
    }

    #[test]
    fn a_cycle_is_refused() {
        let error = plan(
            &[],
            &[
                ("alpha", "-- requires: beta\nSELECT 1;"),
                ("beta", "-- requires: alpha\nSELECT 1;"),
                ("gamma", "SELECT 1;"),
            ],
        )
        .unwrap_err();
        assert!(error.contains("alpha, beta require each other"), "{error}");
        let error = plan(&[], &[("alpha", "-- requires: alpha\nSELECT 1;")]).unwrap_err();
        assert!(error.contains("alpha require each other"), "{error}");
    }

    #[test]
    fn a_name_outside_the_rules_is_refused() {
        for name in ["01_x", "Alpha", "a-b", "_a", ""] {
            assert!(plan(&[], &[(name, "SELECT 1;")]).is_err(), "{name}");
        }
        assert!(plan(&["02_initial"], &[]).is_err());
        assert!(plan(&["1_initial"], &[]).is_err());
    }

    #[test]
    fn only_a_first_line_requires() {
        let planned = plan(
            &[],
            &[
                ("alpha", "SELECT 1;\n-- requires: beta"),
                ("beta", "SELECT 1;"),
            ],
        )
        .unwrap();
        assert_eq!(names(&planned), ["alpha", "beta"]);
    }
}
