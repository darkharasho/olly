//! Applies engine actions to the system.

use crate::engine::Action;

pub trait Actuator {
    fn apply(&mut self, a: &Action) -> Result<(), String>;
}

/// Records actions without touching the system.
#[derive(Default)]
pub struct DryRunActuator {
    pub log: Vec<Action>,
}

impl Actuator for DryRunActuator {
    fn apply(&mut self, a: &Action) -> Result<(), String> {
        self.log.push(a.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Target;
    use crate::sample::{AppKey, TargetKind};

    #[test]
    fn dry_run_actuator_records() {
        let target = Target {
            id: 1,
            key: AppKey { cgroup: "/x".into(), root_pid: None },
            name: "x".into(),
            kind: TargetKind::Cgroup,
            bytes: 1,
        };
        let a = Action::Freeze { target };
        let mut d = DryRunActuator::default();
        assert_eq!(d.apply(&a), Ok(()));
        assert_eq!(d.log, vec![a]);
    }
}
