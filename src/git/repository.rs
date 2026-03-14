use crate::git::backend_router::{GitLocalOps, GitRemoteOps};

/// Combined trait for backends that implement both local and remote operations.
///
/// `RealGitRepository` and `FakeGitRepository` implement this automatically
/// via the blanket impl. Code that needs a single trait object covering all
/// non-forge operations can use `dyn GitRepository`.
pub trait GitRepository: GitLocalOps + GitRemoteOps {}

/// Blanket impl: any type implementing both sub-traits is a GitRepository.
impl<T: GitLocalOps + GitRemoteOps> GitRepository for T {}
