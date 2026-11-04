use crate::{Result, WinError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Lift,
    Install(usize),
    Finalize,
    Verify,
}

pub(super) fn execute(
    installs: usize,
    mut operation: impl FnMut(Phase) -> Result<()>,
    mut restore: impl FnMut() -> Result<()>,
    mut verify_restored: impl FnMut() -> Result<()>,
) -> Result<()> {
    let outcome = (|| {
        operation(Phase::Lift)?;
        for index in 0..installs {
            operation(Phase::Install(index))?;
        }
        operation(Phase::Finalize)?;
        operation(Phase::Verify)
    })();
    if let Err(original) = outcome {
        // Reread even when restoration reports an error: neither result substitutes for the other.
        let restoration = restore();
        let verification = verify_restored();
        return Err(WinError::Verification(format!(
            "{original}; policy restoration: {}; restoration verification: {}",
            restoration.map_or_else(|e| e.to_string(), |()| "writes completed".into()),
            verification.map_or_else(
                |e| e.to_string(),
                |()| "prior state confirmed by reread".into()
            ),
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn every_failure_restores_and_rereads_and_stops_installs() {
        for fail in [
            Phase::Lift,
            Phase::Install(0),
            Phase::Install(1),
            Phase::Finalize,
            Phase::Verify,
        ] {
            for restore_fails in [false, true] {
                for verify_fails in [false, true] {
                    let calls = RefCell::new(Vec::new());
                    let state =
                        RefCell::new(vec!["absent key", "absent value", "foreign entry 99"]);
                    let prior = state.borrow().clone();
                    let result = execute(
                        3,
                        |phase| {
                            calls.borrow_mut().push(format!("{phase:?}"));
                            state.borrow_mut().clear(); // also models a partial lift/write
                            if phase == fail {
                                Err(WinError::Verification("original failure".into()))
                            } else {
                                Ok(())
                            }
                        },
                        || {
                            calls.borrow_mut().push("restore".into());
                            if restore_fails {
                                Err(WinError::Verification("restore failed".into()))
                            } else {
                                *state.borrow_mut() = prior.clone();
                                Ok(())
                            }
                        },
                        || {
                            calls.borrow_mut().push("reread".into());
                            if verify_fails || *state.borrow() != prior {
                                Err(WinError::Verification("reread failed".into()))
                            } else {
                                Ok(())
                            }
                        },
                    );
                    let error = result.unwrap_err().to_string();
                    assert!(error.contains("original failure"));
                    assert_eq!(
                        &calls.borrow()[calls.borrow().len() - 2..],
                        ["restore", "reread"]
                    );
                    if fail == Phase::Install(1) {
                        assert!(!calls.borrow().iter().any(|c| c == "Install(2)"));
                    }
                    if restore_fails {
                        assert!(error.contains("restore failed"));
                    }
                    if verify_fails || restore_fails {
                        assert!(error.contains("reread failed"));
                    }
                }
            }
        }
    }

    #[test]
    fn success_does_not_restore() {
        let mut phases = Vec::new();
        execute(
            2,
            |phase| {
                phases.push(phase);
                Ok(())
            },
            || panic!("restore"),
            || panic!("reread"),
        )
        .unwrap();
        assert_eq!(
            phases,
            [
                Phase::Lift,
                Phase::Install(0),
                Phase::Install(1),
                Phase::Finalize,
                Phase::Verify
            ]
        );
    }
}
