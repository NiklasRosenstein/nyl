//! Bound every subprocess to the remaining time of its enclosing operation.
use nyl_core::orchestration::driver::{DriverResult, ToolExit, ToolInvocation, ToolRunner};
use nyl_core::orchestration::ports::Clock;
use nyl_core::orchestration::time::Timestamp;

/// A driver cannot renew its deadline by starting another native tool.
pub struct DeadlineRunner<'a> {
    pub tools: &'a dyn ToolRunner,
    pub clock: &'a dyn Clock,
    pub deadline: Timestamp,
}

impl ToolRunner for DeadlineRunner<'_> {
    fn run(&self, mut invocation: ToolInvocation) -> DriverResult<ToolExit> {
        let Ok(remaining) = (self.deadline - self.clock.now()).to_std() else {
            return Ok(ToolExit::DeadlineExceeded);
        };
        if remaining.is_zero() {
            return Ok(ToolExit::DeadlineExceeded);
        }
        invocation.timeout = Some(invocation.timeout.map_or(remaining, |timeout| timeout.min(remaining)));
        self.tools.run(invocation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeClock;
    use nyl_core::orchestration::time::Duration;
    use std::sync::Mutex;

    struct Tools(Mutex<Vec<std::time::Duration>>);
    impl ToolRunner for Tools {
        fn run(&self, invocation: ToolInvocation) -> DriverResult<ToolExit> {
            self.0.lock().unwrap().push(invocation.timeout.unwrap());
            Ok(ToolExit::Exited {
                code: 0,
                stdout: vec![],
                stderr: vec![],
            })
        }
    }
    #[test]
    fn test_subprocesses_share_one_deadline_and_cannot_start_after_it() {
        let now = "2026-10-28T00:00:00Z".parse().unwrap();
        let clock = FakeClock::new(now);
        let tools = Tools(Mutex::new(vec![]));
        let runner = DeadlineRunner {
            tools: &tools,
            clock: &clock,
            deadline: Duration::from_secs(10).after(now),
        };
        let invocation = || ToolInvocation {
            program: "native".into(),
            args: vec![],
            working_dir: "/".into(),
            env: std::collections::BTreeMap::default(),
            stdin: None,
            private_stdout: false,
            interactive: false,
            forward_stdout: false,
            timeout: Some(std::time::Duration::from_secs(10)),
            masked: vec![],
        };
        runner.run(invocation()).unwrap();
        clock.advance(Duration::from_secs(7));
        runner.run(invocation()).unwrap();
        clock.advance(Duration::from_secs(3));
        assert_eq!(runner.run(invocation()).unwrap(), ToolExit::DeadlineExceeded);
        assert_eq!(
            *tools.0.lock().unwrap(),
            [std::time::Duration::from_secs(10), std::time::Duration::from_secs(3)]
        );
    }
}
