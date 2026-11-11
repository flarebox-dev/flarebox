// SPDX-License-Identifier: Apache-2.0

//! The coarse facts about the machine that every report carries.

use std::fs;

use crate::schema::{Arch, Build, Client, Distro, Os, OsVersion, Version};

/// Who sends a report, in terms that fit thousands of machines alike.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Environment {
    pub client: Client,
    pub koi_version: Version,
    pub build: Build,
    pub os: Os,
    pub os_version: Option<OsVersion>,
    pub distro: Option<Distro>,
    pub arch: Arch,
}

impl Environment {
    /// The running machine. `koi_version` is the caller's `CARGO_PKG_VERSION`; one that is not a version number is
    /// reported as `0`.
    pub fn detect(client: Client, koi_version: &str) -> Self {
        let os = if cfg!(target_os = "macos") {
            Os::Macos
        } else if cfg!(target_os = "linux") {
            Os::Linux
        } else {
            Os::Other
        };
        let (os_version, distro) = match os {
            Os::Macos => (
                fs::read_to_string(MACOS_VERSION)
                    .ok()
                    .and_then(|plist| macos_version(&plist)),
                None,
            ),
            Os::Linux => match fs::read_to_string(OS_RELEASE) {
                Ok(release) => {
                    let (version, distro) = os_release(&release);
                    (version, Some(distro))
                }
                Err(_) => (None, Some(Distro::Other)),
            },
            Os::Other => (None, None),
        };
        Environment {
            client,
            koi_version: Version::parse(koi_version)
                .unwrap_or_else(|_| Version::parse("0").expect("0 is a version")),
            build: if cfg!(debug_assertions) {
                Build::Dev
            } else {
                Build::Release
            },
            os,
            os_version,
            distro,
            arch: if cfg!(target_arch = "aarch64") {
                Arch::Arm64
            } else if cfg!(target_arch = "x86_64") {
                Arch::X86_64
            } else {
                Arch::Other
            },
        }
    }
}

const MACOS_VERSION: &str = "/System/Library/CoreServices/SystemVersion.plist";
const OS_RELEASE: &str = "/etc/os-release";

/// `ProductVersion` of SystemVersion.plist, such as `15.4.1`, as major and minor.
fn macos_version(plist: &str) -> Option<OsVersion> {
    let after = plist.split("<key>ProductVersion</key>").nth(1)?;
    let value = after.split("<string>").nth(1)?.split("</string>").next()?;
    numbers(value)
}

/// `ID` and `VERSION_ID` of os-release. A distribution not in [`Distro`] is `other`, and its version is dropped,
/// since a rare distribution with its version could tell one machine apart.
fn os_release(release: &str) -> (Option<OsVersion>, Distro) {
    let field = |name: &str| {
        release.lines().find_map(|line| {
            let value = line.strip_prefix(name)?.strip_prefix('=')?;
            Some(value.trim().trim_matches('"').to_owned())
        })
    };
    let distro = match field("ID").as_deref() {
        Some("alpine") => Distro::Alpine,
        Some("arch") => Distro::Arch,
        Some("debian") => Distro::Debian,
        Some("fedora") => Distro::Fedora,
        Some("nixos") => Distro::Nixos,
        Some("opensuse-leap" | "opensuse-tumbleweed") => Distro::Opensuse,
        Some("rhel" | "centos" | "rocky" | "almalinux") => Distro::Rhel,
        Some("ubuntu") => Distro::Ubuntu,
        _ => return (None, Distro::Other),
    };
    (field("VERSION_ID").as_deref().and_then(numbers), distro)
}

fn numbers(value: &str) -> Option<OsVersion> {
    let mut parts = value.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |minor| minor.parse().ok())?;
    Some(OsVersion { major, minor })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_macos_version_comes_from_product_version() {
        let plist = "<dict><key>ProductBuildVersion</key><string>24E263</string>\n<key>ProductVersion</key>\n\t<string>15.4.1</string></dict>";
        assert_eq!(
            macos_version(plist),
            Some(OsVersion {
                major: 15,
                minor: 4
            })
        );
        assert_eq!(macos_version("<dict></dict>"), None);
    }

    #[test]
    fn a_known_distribution_keeps_its_version_and_an_unknown_one_loses_it() {
        let ubuntu = "NAME=\"Ubuntu\"\nID=ubuntu\nVERSION_ID=\"24.04\"\n";
        assert_eq!(
            os_release(ubuntu),
            (
                Some(OsVersion {
                    major: 24,
                    minor: 4
                }),
                Distro::Ubuntu
            )
        );
        let debian = "ID=debian\nVERSION_ID=\"13\"\n";
        assert_eq!(
            os_release(debian),
            (
                Some(OsVersion {
                    major: 13,
                    minor: 0
                }),
                Distro::Debian
            )
        );
        let rare = "ID=alices-own-linux\nVERSION_ID=\"7.1\"\n";
        assert_eq!(os_release(rare), (None, Distro::Other));
    }
}
