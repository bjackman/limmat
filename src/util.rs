use core::fmt;
use std::{
    fmt::{Display, Formatter},
    future::Future,
    io,
    ops::Deref,
    path::PathBuf,
    str::FromStr,
};

#[allow(unused_imports)]
use log::{debug, error};
use sha3::digest;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct DisplayablePathBuf(pub PathBuf);

impl FromStr for DisplayablePathBuf {
    type Err = <PathBuf as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        PathBuf::from_str(s).map(Self)
    }
}

impl From<PathBuf> for DisplayablePathBuf {
    fn from(p: PathBuf) -> Self {
        Self(p)
    }
}

impl From<DisplayablePathBuf> for PathBuf {
    fn from(d: DisplayablePathBuf) -> PathBuf {
        d.0
    }
}

impl Display for DisplayablePathBuf {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        Display::fmt(&self.0.display(), f)
    }
}

impl Deref for DisplayablePathBuf {
    type Target = PathBuf;

    fn deref(&self) -> &PathBuf {
        &self.0
    }
}

pub trait ResultExt {
    // Log an error if it occurs, prefixed with s, otherwise return nothing.
    fn or_log_error(&self, s: &str);
}

impl<T, E> ResultExt for Result<T, E>
where
    E: Display,
{
    fn or_log_error(&self, s: &str) {
        if let Err(e) = self {
            error!("{} - {}", s, e);
        }
    }
}

// It's an ErrGroup like from Go lol.
// https://stackoverflow.com/questions/79172707/concise-tokio-equivalent-of-gos-errgroup
pub struct ErrGroup {
    ct: CancellationToken,
    join_set: JoinSet<anyhow::Result<()>>,
}

impl ErrGroup {
    pub fn new(ct: CancellationToken) -> Self {
        Self {
            ct,
            join_set: JoinSet::new(),
        }
    }

    pub fn spawn<F>(&mut self, task: F)
    where
        F: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        // Drop the returned AbortHandle so we can unwrap the result of the join in wait.
        self.join_set.spawn(task);
    }

    // Block until all tasks are complete, return the first error. As soon as
    // any returns an error, cancel the token passed to new. Panics if any of
    // the tasks panic.
    pub async fn wait(mut self) -> anyhow::Result<()> {
        let mut final_result: anyhow::Result<()> = Ok(());

        while let Some(result) = self.join_set.join_next().await {
            if let Err(err) = result.expect("joining ErrGroup tasks") {
                if final_result.is_ok() {
                    final_result = Err(err)
                }
                self.ct.cancel();
                break;
            }
        }

        // Wait for remaining tasks to exit due to cancellation
        let _ = self.join_set.join_all().await;

        final_result
    }
}

#[derive(Clone)]
pub struct Rect {
    pub cols: usize,
    pub rows: usize,
}

pub trait IoResultExt {
    fn ignore(self, kind: io::ErrorKind) -> Self;
}

impl IoResultExt for io::Result<()> {
    fn ignore(self, kind: io::ErrorKind) -> io::Result<()> {
        match self {
            Err(e) => {
                if e.kind() == kind {
                    Ok(())
                } else {
                    Err(e)
                }
            }
            Ok(()) => Ok(()),
        }
    }
}

// I want to use the RustCrypto hasher types as a Hasher (i.e. on objects that
// don't actually provide bytes). I suspect the fact that this isn't
// well-supported means it's a terrible idea in general. I don't really know why
// that is, but it's certainly harmless here. So, this is an adapter for making
// a std::hash::Hasher from a digest::Digest.
pub struct DigestHasher<D: digest::Digest> {
    pub digest: D,
}

impl<D: digest::Digest> std::hash::Hasher for DigestHasher<D> {
    fn write(&mut self, bytes: &[u8]) {
        self.digest.update(bytes)
    }

    // This is required for the Hasher trait, but you shouldn't call it, it's
    // just throwing hash bits away for no reason.
    fn finish(&self) -> u64 {
        panic!("don't call this");
    }
}
