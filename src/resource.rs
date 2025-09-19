use std::collections::HashMap;
use std::mem::ManuallyDrop;

use async_condvar_fair::Condvar;
#[allow(unused_imports)]
use log::debug;
use parking_lot::Mutex;

use crate::git::TempWorktree;

// Key to identify the type of resource that can be put into the pool.
#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub enum ResourceKey {
    // Special type of resource representing worktrees. This code doesn't
    // actually care about worktrees so probably we should actually just be
    // generic over the key type.
    Worktree,
    UserToken(String), // Resource defined by the user.
}

// Resource that can be put in the pool. This is another thing where we leak the
// details of the user into this code, we probably shouldn't know about
// TempWorktree in here.
#[derive(Debug)]
pub enum Resource {
    Worktree(TempWorktree),
    UserToken(String),
}

impl Resource {
    // Assumes that your resource is a worktree, panics if it isn't.
    pub fn as_worktree(&self) -> &TempWorktree {
        match self {
            Self::Worktree(w) => w,
            _ => panic!("as_worktree called on bogus Resource"),
        }
    }
}

// Collection of shared resources, consisting of pools of resources. The
// user can block until an arbitrary combination of numbers of different tokens
// becomes available, without any underutilization or deadlocking. Tokens are
// strings, which is another thing this code doesn't actually care about and
// probably "should" be generic over.
#[derive(Debug)]
pub struct Pools {
    cond: Condvar,
    resources: Mutex<HashMap<ResourceKey, Vec<Resource>>>,
}

impl Pools {
    // Create a collection of pools where sizes specifies the initial number of tokens in each
    // pool.
    // TODO: this key/val tuple approach is kinda annoying, maybe we should have
    // a trait object that implements Into<Resource> or something?
    pub fn new(resources: impl IntoIterator<Item = (ResourceKey, Vec<Resource>)>) -> Self {
        Self {
            cond: Condvar::new(),
            resources: Mutex::new(resources.into_iter().collect()),
        }
    }

    // TODO: As well as being annoying in a similar way to new, this is
    // inconsistent with it for no good reason.
    pub fn add(&self, new_resources: impl IntoIterator<Item = (ResourceKey, Resource)>) {
        // Don't need the condvar since we have a mutable reference to self. We
        // only take the mutex out of a misguided sense of decorum.
        let mut resources = self.resources.lock();
        for (key, resource) in new_resources.into_iter() {
            (*resources).entry(key).or_default().push(resource);
        }
        self.cond.notify_all();
    }

    // Get the specified number of tokens from each of the pools, keys match
    // the keys used in new (or this panics).
    // The tokens are held until you drop the returned value.
    //
    // https://github.com/rust-lang/rust-clippy/issues/13075
    #[expect(clippy::await_holding_lock)]
    pub async fn get(
        &self,
        wants: impl IntoIterator<Item = (ResourceKey, usize)>,
    ) -> Resources<'_> {
        let wants: Vec<(ResourceKey, usize)> = wants.into_iter().collect();
        let mut guard = self.resources.lock();
        loop {
            let avail_tokens = &mut (*guard);
            // For simplicity we first iterate to check if all the resources we
            // need are available, then if they are we take them out in a
            // separate operation.
            if wants
                .iter()
                .all(|(key, want)| avail_tokens.get(key).unwrap_or(&vec![]).len() >= *want)
            {
                return Resources {
                    resources: ManuallyDrop::new(
                        wants
                            .into_iter()
                            .map(|(key, want_count)| {
                                let avail =
                                    avail_tokens.get_mut(&key).expect("invalid resource key");
                                // Take the last n tokens out of the Vec and
                                // associated them with the key.
                                (key, avail.drain((avail.len() - want_count)..).collect())
                            })
                            .collect(),
                    ),
                    pools: self,
                };
            }

            guard = self.cond.wait(guard).await;
        }
    }

    // Without blocking, permanently remove all the worktrees that are currently available.
    // specified type that are currently available, up to the specified number.
    pub fn try_remove_worktrees(&self) -> impl Iterator<Item = TempWorktree> {
        let mut guard = self.resources.lock();
        let avail = &mut (*guard);
        avail
            .remove(&ResourceKey::Worktree)
            .unwrap_or_default()
            .into_iter()
            .map(|resource| match resource {
                Resource::Worktree(w) => w,
                _ => panic!("wrong resource type in worktree pool"),
            })
    }

    fn put(&self, resources: HashMap<ResourceKey, Vec<Resource>>) {
        let mut guard = self.resources.lock();
        let avail_tokens = &mut (*guard);
        for (key, mut key_resources) in resources.into_iter() {
            avail_tokens
                .get_mut(&key)
                .expect("invalid resource key")
                .append(&mut key_resources);
        }
        // Note this is pretty inefficient, we are waking up every getter even though we can satisfy
        // at most one of them.
        self.cond.notify_all();
    }
}

#[derive(Debug)]
// Tokens taken from a Pools.
pub struct Resources<'a> {
    resources: ManuallyDrop<HashMap<ResourceKey, Vec<Resource>>>,
    pools: &'a Pools,
}

impl Resources<'_> {
    // Get access to the resources with the given key.
    pub fn resources(&self, key: &ResourceKey) -> Option<&Vec<Resource>> {
        self.resources.get(key)
    }

    // Get all the user-configured token values
    pub fn tokens(&self) -> HashMap<String, Vec<String>> {
        self.resources
            .iter()
            .filter_map(|(key, tokens)| match key {
                ResourceKey::UserToken(name) => Some((
                    name.clone(),
                    tokens
                        .iter()
                        .map(|t| match t {
                            Resource::UserToken(token) => token.clone(),
                            _ => panic!("bad token type for UserToken resource key"),
                        })
                        .collect(),
                )),
                _ => None,
            })
            .collect()
    }
}

impl Drop for Resources<'_> {
    fn drop(&mut self) {
        // SAFETY: This is safe as the fields are never accessed again.
        let resources = unsafe { ManuallyDrop::take(&mut self.resources) };
        self.pools.put(resources)
    }
}

#[cfg(test)]
mod tests {
    use anyhow::bail;
    use std::task::{Context, Poll};

    use futures::{pin_mut, task::noop_waker, Future};
    use test_case::test_case;

    use super::*;

    // Assert that a future is blocked. Note that panicking directly in assertion helpers like this
    // is unhelpful because you lose line number debug. It seems the proper solution for that is to
    // make them macros instead of functions. My solution is instead to just return errors and then
    // .expect() them, because I don't know how to make macros.
    fn check_pending<F>(fut: F) -> anyhow::Result<()>
    where
        F: Future,
        <F as futures::Future>::Output: std::fmt::Debug,
    {
        pin_mut!(fut);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // Poll the future before it completes
        match fut.as_mut().poll(&mut cx) {
            Poll::Pending => Ok(()),
            Poll::Ready(res) => bail!("The future should be pending, but it produced {:?}", res),
        }
    }

    #[test_case(
        vec![("foo".into(), vec![]), ("bar".into(), vec![])],
        vec![("foo".into(), 1), ("bar".into(), 0)]; "two empty")]
    // #[test_case(vec![0, 0], 1, vec![1, 1] ; "two empty, want both")]
    // #[test_case(vec![4], 1, vec![6] ; "too many")]
    // #[test_case(vec![0], 1, vec![1] ; "one empty")]
    // #[test_case(vec![4], 0, vec![1] ; "no objs")]
    fn test_pools_one_empty_blocks(
        resources: Vec<(String, Vec<String>)>,
        wants: Vec<(String, usize)>,
    ) {
        let pools = Pools::new(resources.into_iter().map(|(key, tokens)| {
            (
                ResourceKey::UserToken(key),
                tokens.into_iter().map(Resource::UserToken).collect(),
            )
        }));
        check_pending(
            pools.get(
                wants
                    .into_iter()
                    .map(|(k, n)| (ResourceKey::UserToken(k), n)),
            ),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn test_pools_get_some() {
        let pools = Pools::new([
            (
                ResourceKey::UserToken("foo".into()),
                vec![
                    Resource::UserToken("foo1".into()),
                    Resource::UserToken("foo2".into()),
                    Resource::UserToken("foo3".into()),
                ],
            ),
            (
                ResourceKey::UserToken("bar".into()),
                vec![
                    Resource::UserToken("bar1".into()),
                    Resource::UserToken("bar2".into()),
                ],
            ),
        ]);
        {
            let _tokens = pools
                .get([
                    (ResourceKey::UserToken("foo".into()), 2),
                    (ResourceKey::UserToken("bar".into()), 2),
                ])
                .await;
            check_pending(pools.get([(ResourceKey::UserToken("foo".into()), 3)]))
                .expect("returned too many tokens");
        }
        pools.get([(ResourceKey::UserToken("foo".into()), 3)]).await;
    }
}
