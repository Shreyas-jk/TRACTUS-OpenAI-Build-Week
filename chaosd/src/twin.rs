use chaos_core::contract::{Effects, Reason};
use chaos_core::parse::SimpleCommand;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

pub enum TwinOutcome {
    Effects(Effects),
    NeedsHuman(Reason),
}

pub trait TwinExecutor: Send + Sync {
    fn speculate<'a>(
        &'a self,
        cmd: &'a SimpleCommand,
        cwd: &'a Path,
    ) -> Pin<Box<dyn Future<Output = TwinOutcome> + Send + 'a>>;
}

#[derive(Default)]
pub struct NoTwin;

impl TwinExecutor for NoTwin {
    fn speculate<'a>(
        &'a self,
        _cmd: &'a SimpleCommand,
        _cwd: &'a Path,
    ) -> Pin<Box<dyn Future<Output = TwinOutcome> + Send + 'a>> {
        Box::pin(async { TwinOutcome::NeedsHuman(Reason::Opaque) })
    }
}
