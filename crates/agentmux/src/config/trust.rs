//! Whether a configuration file is one only its owner could have written.

use std::path::Path;

use super::ConfigError;

/// Refuse a configuration file that another user owns or could write.
///
/// It can name commands, endpoints and credentials, so a writable file is an injection point
/// rather than merely an exposed one — the same reason `ssh` refuses a group-writable config.
/// Ownership matters for the same reason: a project walk can pass through directories other
/// users write to, and a file they own with tidy permissions is still theirs.
/// Readability is not checked: `0644` is normal inside a container image, and failing a delegation
/// over it would cost a turn for no gain.
///
/// A group-writable file is accepted when its owner is the group's only member (see
/// [`is_owners_alone`]).
#[cfg(unix)]
pub(super) fn refuse_if_untrusted(path: &Path, file: &std::fs::File) -> Result<(), ConfigError> {
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::fs::PermissionsExt as _;

    let Ok(metadata) = file.metadata() else {
        // An unreadable file is reported by the read that follows, with a better message.
        return Ok(());
    };
    let file = FileTrust {
        is_file: metadata.is_file(),
        mode: metadata.permissions().mode(),
        owner_uid: metadata.uid(),
        reader_uid: nix::unistd::geteuid().as_raw(),
    };
    let gid = metadata.gid();
    file.verdict(|| group_of(gid, file.owner_uid).is_some_and(|group| is_owners_alone(&group)))
        .map_err(|problem| ConfigError::Untrusted {
            path: path.to_path_buf(),
            problem,
        })
}

/// Permission bits and ownership do not carry the same meaning on Windows, so only the shape is
/// checked there.
#[cfg(not(unix))]
pub(super) fn refuse_if_untrusted(path: &Path, file: &std::fs::File) -> Result<(), ConfigError> {
    if file.metadata().is_ok_and(|metadata| !metadata.is_file()) {
        return Err(ConfigError::Untrusted {
            path: path.to_path_buf(),
            problem: "is not a regular file",
        });
    }
    Ok(())
}

/// What the trust decision reads from a file's metadata.
#[cfg(unix)]
struct FileTrust {
    is_file: bool,
    mode: u32,
    owner_uid: u32,
    /// The user agentmux runs as.
    reader_uid: u32,
}

#[cfg(unix)]
impl FileTrust {
    /// Why the file is refused, if it is.
    ///
    /// `owner_is_alone_in_group` is asked only for a file whose sole remaining problem is its group
    /// write bit, so a file that needs no exception costs no account lookup.
    fn verdict(&self, owner_is_alone_in_group: impl FnOnce() -> bool) -> Result<(), &'static str> {
        if !self.is_file {
            return Err("is not a regular file");
        }
        if self.owner_uid != self.reader_uid {
            return Err("is owned by another user");
        }
        if self.mode & 0o002 != 0 {
            return Err("is writable by other users");
        }
        if self.mode & 0o020 != 0 && !owner_is_alone_in_group() {
            return Err("is writable by a group other users belong to");
        }
        Ok(())
    }
}

/// Who belongs to a file's group, by both routes a user can.
#[cfg(unix)]
struct GroupOfFile {
    owner_uid: u32,
    owner_name: String,
    /// The members the group database lists.
    listed_members: Vec<String>,
    /// Every account whose primary group this is, whether or not it is listed.
    primary_members: Vec<u32>,
}

/// Look up the file's group and its owner, or `None` when either cannot be found.
#[cfg(unix)]
fn group_of(file_gid: u32, owner_uid: u32) -> Option<GroupOfFile> {
    use nix::unistd::{Gid, Group, Uid, User};

    let group = Group::from_gid(Gid::from_raw(file_gid)).ok()??;
    let owner = User::from_uid(Uid::from_raw(owner_uid)).ok()??;
    Some(GroupOfFile {
        owner_uid,
        owner_name: owner.name,
        listed_members: group.mem,
        primary_members: accounts_with_primary_group(file_gid),
    })
}

/// Whether a file's owner is the only member of its group, which makes the group write bit grant
/// nobody else anything.
///
/// This is the rule Debian's OpenSSH applies to `StrictModes`, because distributions that give
/// every user a group of their own, Ubuntu among them, default to umask `002`, and every file such
/// a user writes is group-writable.
/// Every account whose primary group it is must be the owner, the group may list no member but
/// the owner, and it must have at least one member by either route: a group with none is the kind
/// setgid programs use.
///
/// Names are compared as nix decodes them, which replaces bytes that are not UTF-8 with U+FFFD, so
/// two different names could read the same; a name with that replacement character in it is
/// refused rather than compared.
#[cfg(unix)]
fn is_owners_alone(group: &GroupOfFile) -> bool {
    let decoded_faithfully = |name: &str| !name.contains(char::REPLACEMENT_CHARACTER);
    if !decoded_faithfully(&group.owner_name)
        || !group
            .listed_members
            .iter()
            .all(|name| decoded_faithfully(name))
    {
        return false;
    }
    let listed_only_owner = match group.listed_members.as_slice() {
        [] => true,
        [member] => *member == group.owner_name,
        _ => false,
    };
    let primary_only_owner = group
        .primary_members
        .iter()
        .all(|uid| *uid == group.owner_uid);
    let has_member = !group.listed_members.is_empty() || !group.primary_members.is_empty();
    listed_only_owner && primary_only_owner && has_member
}

/// Every account the password database lists whose primary group is `gid`.
///
/// What it lists is what the account databases enumerate: an NSS source that does not enumerate,
/// as SSSD does not by default, contributes none of its accounts, and an error part-way ends the
/// walk as its end does.
/// Debian's check takes the walk as complete in both cases, and so does this one.
///
/// `getpwent` walks one cursor shared by the whole process, so two walks at once would interleave;
/// the lock keeps each walk whole, and nothing else in agentmux uses that cursor.
#[cfg(unix)]
fn accounts_with_primary_group(gid: u32) -> Vec<u32> {
    static PASSWD_CURSOR: std::sync::Mutex<()> = std::sync::Mutex::new(());

    let cursor = PASSWD_CURSOR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut uids = Vec::new();
    // SAFETY: the lock above gives this walk sole use of the process's password-database cursor.
    // Each entry `getpwent` returns stays valid until its next call, and only its plain integer
    // fields are read before then.
    #[expect(
        unsafe_code,
        reason = "nix wraps getpwuid and getgrgid but not the getpwent walk that finds every \
                  account with a given primary group"
    )]
    unsafe {
        nix::libc::setpwent();
        loop {
            let entry = nix::libc::getpwent();
            if entry.is_null() {
                break;
            }
            if (*entry).pw_gid == gid {
                uids.push((*entry).pw_uid);
            }
        }
        nix::libc::endpwent();
    }
    drop(cursor);
    uids
}

#[cfg(all(test, unix))]
mod tests {
    use googletest::prelude::*;

    use super::{FileTrust, GroupOfFile, accounts_with_primary_group, is_owners_alone};

    const ALICE: u32 = 1000;
    const BOB: u32 = 1001;

    fn alices_file(mode: u32) -> FileTrust {
        FileTrust {
            is_file: true,
            mode,
            owner_uid: ALICE,
            reader_uid: ALICE,
        }
    }

    fn alices_group(listed_members: &[&str], primary_members: &[u32]) -> GroupOfFile {
        GroupOfFile {
            owner_uid: ALICE,
            owner_name: "alice".to_owned(),
            listed_members: listed_members
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            primary_members: primary_members.to_vec(),
        }
    }

    /// A user-private group excuses the group write bit; a group anyone else belongs to does not.
    #[gtest]
    fn a_group_write_bit_is_excused_only_for_the_owners_own_group() {
        let file = alices_file(0o664);

        assert_that!(file.verdict(|| true), ok(eq(())));
        assert_that!(
            file.verdict(|| false),
            err(eq("is writable by a group other users belong to"))
        );
    }

    /// No group can excuse a world write bit, another owner or a non-file, and none is looked up.
    #[gtest]
    fn other_problems_are_refused_before_the_group_is_asked_about() {
        let looked_up = std::cell::Cell::new(false);
        let never = || {
            looked_up.set(true);
            true
        };

        assert_that!(
            alices_file(0o666).verdict(never),
            err(eq("is writable by other users"))
        );
        assert_that!(
            FileTrust {
                reader_uid: BOB,
                ..alices_file(0o600)
            }
            .verdict(never),
            err(eq("is owned by another user"))
        );
        assert_that!(
            FileTrust {
                is_file: false,
                ..alices_file(0o600)
            }
            .verdict(never),
            err(eq("is not a regular file"))
        );
        assert_that!(alices_file(0o644).verdict(never), ok(eq(())));
        assert_that!(looked_up.get(), eq(false));
    }

    /// The owner alone, by primary group, by listing, or both, is a user-private group.
    #[gtest]
    fn the_owner_alone_is_a_user_private_group() {
        assert_that!(is_owners_alone(&alices_group(&[], &[ALICE])), eq(true));
        assert_that!(is_owners_alone(&alices_group(&["alice"], &[])), eq(true));
        assert_that!(
            is_owners_alone(&alices_group(&["alice"], &[ALICE])),
            eq(true)
        );
    }

    /// Another account in the group by either route, or a group with no member at all, is shared.
    #[gtest]
    fn any_other_member_or_none_makes_the_group_shared() {
        // Bob's primary group is Alice's, though the group database lists nobody.
        assert_that!(
            is_owners_alone(&alices_group(&[], &[ALICE, BOB])),
            eq(false)
        );
        assert_that!(
            is_owners_alone(&alices_group(&["bob"], &[ALICE])),
            eq(false)
        );
        assert_that!(
            is_owners_alone(&alices_group(&["alice", "bob"], &[ALICE])),
            eq(false)
        );
        assert_that!(is_owners_alone(&alices_group(&[], &[])), eq(false));
    }

    /// Names nix had to decode lossily are refused, because two different names can read the same.
    #[gtest]
    fn a_lossily_decoded_name_is_never_trusted() {
        let lossy = "a\u{FFFD}";
        assert_that!(
            is_owners_alone(&GroupOfFile {
                owner_name: lossy.to_owned(),
                ..alices_group(&[lossy], &[ALICE])
            }),
            eq(false)
        );
        assert_that!(
            is_owners_alone(&alices_group(&["alice", lossy], &[ALICE])),
            eq(false)
        );
    }

    /// The walk finds the running user among the accounts of the primary group its own entry names.
    ///
    /// Opt-in, because it holds only where the account databases enumerate the running user, which
    /// a directory-backed account on a domain workstation may not be.
    #[gtest]
    #[ignore = "needs an account database that enumerates the running user; run with --run-ignored"]
    fn the_walk_finds_the_running_user_in_its_primary_group() -> Result<()> {
        let uid = nix::unistd::getuid();
        let user = nix::unistd::User::from_uid(uid).or_fail()?.or_fail()?;

        assert_that!(
            accounts_with_primary_group(user.gid.as_raw()),
            contains(eq(&uid.as_raw()))
        );
        Ok(())
    }
}
