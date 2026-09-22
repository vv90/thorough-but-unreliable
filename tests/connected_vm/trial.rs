//! Pure selection and validation of smoke versus user-supplied trials.
use super::*;
use thorough_but_unreliable::manifest::Manifest;

enum Assessment {
    Smoke,
    Record,
}

// Constructed only after validation. The original JSON is retained verbatim
// so the ISO and saved artifacts describe exactly what the user supplied.
pub(in super::super) struct Trial {
    manifest: String,
    assessment: Assessment,
}

impl Trial {
    pub(in super::super) fn smoke(model: &str) -> TestResult<Self> {
        Ok(Self {
            manifest: manifest(model)?,
            assessment: Assessment::Smoke,
        })
    }

    pub(in super::super) fn supplied(text: String) -> TestResult<Self> {
        let manifest = Manifest::from_slice(text.as_bytes())?;
        if manifest.inference().completion_url().as_str()
            != "http://10.99.1.1:11434/v1/chat/completions"
            || manifest.command().command_url().as_str() != "http://10.99.2.2:8080/v1/command"
        {
            return Err("this runner requires inference at http://10.99.1.1:11434/v1/chat/completions and commands at http://10.99.2.2:8080/v1/command".into());
        }
        Ok(Self {
            manifest: text,
            assessment: Assessment::Record,
        })
    }

    pub(in super::super) fn manifest(&self) -> &str {
        &self.manifest
    }

    pub(in super::super) fn completion_message(&self) -> &'static str {
        match self.assessment {
            Assessment::Smoke => {
                "local inference VM run: PASS (real inference, target command, submission, cleanup)"
            }
            Assessment::Record => {
                "local trial: RECORDED (report retained and cleanup verified; task success not assessed)"
            }
        }
    }

    pub(in super::super) fn verify(&self, report: &Value) -> TestResult {
        match self.assessment {
            Assessment::Smoke => verify_report(report),
            Assessment::Record => {
                verify_recorded(&Manifest::from_slice(self.manifest.as_bytes())?, report)
            }
        }
    }
}

fn verify_recorded(manifest: &Manifest, report: &Value) -> TestResult {
    if report.get("version") != Some(&json!(1))
        || report.get("run_id").and_then(Value::as_str) != Some(manifest.run_id())
    {
        return Err("report version or run ID does not match the supplied manifest".into());
    }
    let history = report
        .get("history")
        .and_then(Value::as_array)
        .ok_or("missing report history")?;
    let mut messages = history.iter();
    if messages.next() != Some(&json!({"role":"system","content":manifest.system_prompt()}))
        || messages.next() != Some(&json!({"role":"user","content":manifest.task()}))
        || history
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
            .count()
            > usize::try_from(manifest.max_model_turns().get())?
    {
        return Err("report prompts or turn count do not match the supplied manifest".into());
    }
    // Check the terminal envelope without interpreting the answer as success.
    let outcome = report.get("outcome").ok_or("missing terminal outcome")?;
    let kind = outcome
        .get("kind")
        .and_then(Value::as_str)
        .ok_or("missing outcome kind")?;
    let call_id = || {
        outcome
            .get("call_id")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
    };
    let error = || outcome.get("error").is_some_and(Value::is_object);
    let valid = match kind {
        "submitted" => outcome.get("answer").is_some_and(Value::is_string),
        "early_termination" | "model_turn_limit" => true,
        "protocol_error" | "model_failure" => error(),
        "command_client_failure" => call_id() && error(),
        "target_session_unusable" => call_id(),
        _ => false,
    };
    if !valid {
        return Err("invalid or unknown terminal outcome".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn report_for(trial: &Trial, outcome: Value) -> TestResult<Value> {
        let manifest = Manifest::from_slice(trial.manifest().as_bytes())?;
        Ok(
            json!({"version":1,"run_id":manifest.run_id(),"outcome":outcome,"history":[
                {"role":"system","content":manifest.system_prompt()},
                {"role":"user","content":manifest.task()}
            ]}),
        )
    }

    proptest! {
        #[test]
        fn supplied_settings_and_original_bytes_are_preserved(
            task in ".{1,100}", model in "[a-z][a-z0-9:.-]{0,40}", turns in 1u32..100,
        ) {
            prop_assume!(!task.trim().is_empty());
            let check = || -> TestResult {
                let mut value: Value = serde_json::from_str(&manifest("initial-model")?)?;
                *value.get_mut("task").ok_or("missing task")? = json!(task);
                *value.get_mut("max_model_turns").ok_or("missing turns")? = json!(turns);
                *value.pointer_mut("/inference/model").ok_or("missing model")? = json!(model);
                let text = serde_json::to_string_pretty(&value)?;
                let trial = Trial::supplied(text.clone())?;
                assert_eq!(trial.manifest(), text);
                let parsed = Manifest::from_slice(trial.manifest().as_bytes())?;
                assert_eq!(parsed.task(), task);
                assert_eq!(parsed.inference().model(), model);
                assert_eq!(parsed.max_model_turns().get(), turns);
                Ok(())
            };
            check().map_err(|e| TestCaseError::fail(e.to_string()))?;
        }

        #[test]
        fn recording_does_not_grade_answers_and_rejects_other_runs(answer in ".{0,100}", suffix in ".{1,40}") {
            let check = || -> TestResult {
                let trial = Trial::supplied(manifest("model")?)?;
                let mut report = report_for(&trial, json!({"kind":"submitted","answer":answer}))?;
                trial.verify(&report)?;
                *report.get_mut("run_id").ok_or("missing run ID")? = json!(format!("local-inference{suffix}"));
                assert!(trial.verify(&report).is_err());
                Ok(())
            };
            check().map_err(|e| TestCaseError::fail(e.to_string()))?;
        }
    }

    #[test]
    fn recorded_terminal_outcomes_do_not_require_submission() -> TestResult {
        let trial = Trial::supplied(manifest("model")?)?;
        for outcome in [
            json!({"kind":"early_termination"}),
            json!({"kind":"model_turn_limit"}),
            json!({"kind":"model_failure","error":{"category":{"kind":"transport"}}}),
            json!({"kind":"protocol_error","error":{"kind":"empty_call_id"}}),
            json!({"kind":"command_client_failure","call_id":"c","error":{"category":{"kind":"transport"}}}),
            json!({"kind":"target_session_unusable","call_id":"c"}),
        ] {
            let report = report_for(&trial, outcome)?;
            trial.verify(&report)?;
            assert!(Trial::smoke("model")?.verify(&report).is_err());
        }
        for outcome in [
            json!({"kind":"unknown"}),
            json!({"kind":"submitted"}),
            json!({"kind":"model_failure"}),
        ] {
            assert!(trial.verify(&report_for(&trial, outcome)?).is_err());
        }
        Ok(())
    }

    #[test]
    fn incompatible_network_and_invalid_manifests_are_rejected() -> TestResult {
        assert!(Trial::supplied("{}".into()).is_err());
        for pointer in ["/inference/completion_url", "/command/command_url"] {
            let mut value: Value = serde_json::from_str(&manifest("model")?)?;
            let url = value
                .pointer(pointer)
                .and_then(Value::as_str)
                .ok_or("missing URL")?
                .replace("10.99.", "10.98.");
            *value.pointer_mut(pointer).ok_or("missing URL")? = json!(url);
            assert!(Trial::supplied(serde_json::to_string(&value)?).is_err());
        }
        Ok(())
    }
}
