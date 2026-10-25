//! OS user lookup (NSS) and spawning processes as a login user.

use std::ffi::{CString, OsString};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};

pub struct OsUser {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
    pub shell: PathBuf,
}

/// Resolve a user via NSS (passwd databases), so libnss-authentik users work.
pub fn lookup(name: &str) -> io::Result<Option<OsUser>> {
    match nix::unistd::User::from_name(name)
        .map_err(|e| io::Error::other(format!("getpwnam({name}): {e}")))?
    {
        Some(u) => Ok(Some(OsUser {
            name: u.name,
            uid: u.uid.as_raw(),
            gid: u.gid.as_raw(),
            home: u.dir,
            shell: u.shell,
        })),
        None => Ok(None),
    }
}

/// Standard login environment for a spawned user process.
pub fn login_env(user: &OsUser, pam_env: &[(OsString, OsString)]) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = vec![
        ("HOME".into(), user.home.clone().into_os_string()),
        ("USER".into(), user.name.clone().into()),
        ("LOGNAME".into(), user.name.clone().into()),
        ("SHELL".into(), user.shell.clone().into_os_string()),
        (
            "XDG_RUNTIME_DIR".into(),
            format!("/run/user/{}", user.uid).into(),
        ),
        (
            "DBUS_SESSION_BUS_ADDRESS".into(),
            format!("unix:path=/run/user/{}/bus", user.uid).into(),
        ),
        (
            "PATH".into(),
            "/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin".into(),
        ),
    ];
    env.extend(pam_env.iter().cloned());
    env
}

/// Run a program fully de-privileged as `user`: fork, initgroups, setgid,
/// setuid, chdir home, controlled environment.
pub fn run_as_user(
    user: &OsUser,
    pam_env: &[(OsString, OsString)],
    program: &str,
    args: &[&str],
) -> io::Result<Child> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .env_clear()
        .envs(login_env(user, pam_env))
        .current_dir(&user.home);
    let cname = CString::new(user.name.clone())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad username"))?;
    let (uid, gid) = (user.uid, user.gid);
    unsafe {
        cmd.pre_exec(move || {
            nix::unistd::initgroups(&cname, nix::unistd::Gid::from_raw(gid))
                .map_err(|e| io::Error::other(format!("initgroups: {e}")))?;
            nix::unistd::setgid(nix::unistd::Gid::from_raw(gid))
                .map_err(|e| io::Error::other(format!("setgid: {e}")))?;
            nix::unistd::setuid(nix::unistd::Uid::from_raw(uid))
                .map_err(|e| io::Error::other(format!("setuid: {e}")))?;
            Ok(())
        });
    }
    cmd.spawn()
}
