//! What one memory action answers: plain text, or the records of a JSON recall (#3369).

pub(crate) use super::recalled::Recalled;

/// What one memory action answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reply {
    /// The text of a call that is not a recall, such as the wake-up pack.
    Text(String),
    /// The records of a recall, in icm's rank order.
    Records(Vec<Recalled>),
}

impl Reply {
    /// The reply as text: the records one per line.
    pub(crate) fn into_text(self) -> String {
        match self {
            Self::Text(text) => text,
            Self::Records(records) => records
                .into_iter()
                .map(|r| r.text)
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    /// The records of a recall reply.
    ///
    /// # Errors
    /// The reply is text. Only a recall action answers records, so text here means that a
    /// caller gave the reply of another action to a recall path.
    pub(crate) fn into_records(self) -> anyhow::Result<Vec<Recalled>> {
        match self {
            Self::Records(records) => Ok(records),
            Self::Text(_) => Err(anyhow::anyhow!(
                "a memory action answered text where recall records were expected; the action \
                 is not a recall. Fix: report this to llmenv"
            )),
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn records_join_as_one_line_each() {
        let reply = Reply::Records(vec![
            Recalled::from_text("[t] a"),
            Recalled::from_text("[t] b"),
        ]);
        assert_eq!(reply.clone().into_text(), "[t] a\n[t] b");
        assert_eq!(reply.into_records().unwrap().len(), 2);
    }

    #[test]
    fn text_is_not_records() {
        let err = Reply::Text("[t] a".into()).into_records().unwrap_err();
        assert!(err.to_string().contains("not a recall"), "{err}");
    }
}
