//! Expand once under a shared pipeline budget, before any command side effects.

use std::borrow::Cow;
use std::collections::BTreeMap;

use crate::shell::{RedirectTarget, Segment, SimpleCommand, Word};

pub(super) struct ExpandedCommand<'a> {
    pub syntax: &'a SimpleCommand,
    pub env: BTreeMap<String, String>,
    pub words: Vec<String>,
    pub targets: Vec<Vec<String>>,
}

pub(super) struct Budget(pub usize);

impl Budget {
    fn charge(&mut self, bytes: usize) -> Option<()> {
        self.0 = self.0.checked_sub(bytes)?;
        Some(())
    }

    pub fn expand<'a>(
        &mut self,
        syntax: &'a SimpleCommand,
        base: &BTreeMap<String, String>,
        status: i32,
    ) -> Option<ExpandedCommand<'a>> {
        self.charge(std::mem::size_of::<ExpandedCommand<'_>>())?;
        for (name, value) in base {
            self.charge(2 * std::mem::size_of::<String>())?;
            self.charge(name.len())?;
            self.charge(value.len())?;
        }
        let mut env = base.clone();
        // Assignment RHSs see earlier assignments in the same command. Args
        // still use the original environment, as do non-null redirects.
        for assignment in &syntax.assignments {
            self.charge(std::mem::size_of::<String>())?;
            self.charge(assignment.name.len())?;
            let value = self.word(&assignment.value, &env, status, false)?.pop()?;
            env.insert(assignment.name.clone(), value);
        }
        let mut words = Vec::new();
        for word in &syntax.words {
            words.extend(self.word(word, base, status, true)?);
        }
        let redirect_env = if words.is_empty() { &env } else { base };
        let mut targets = Vec::new();
        for redirect in &syntax.redirects {
            self.charge(std::mem::size_of::<Vec<String>>())?;
            targets.push(match &redirect.target {
                RedirectTarget::Word(word) => self.word(word, redirect_env, status, true)?,
                RedirectTarget::Fd(_) => Vec::new(),
            });
        }
        Some(ExpandedCommand {
            syntax,
            env,
            words,
            targets,
        })
    }

    fn word(
        &mut self,
        word: &Word,
        env: &BTreeMap<String, String>,
        status: i32,
        split: bool,
    ) -> Option<Vec<String>> {
        let mut fields = Vec::new();
        let mut current = String::new();
        let mut keep = !split;
        for segment in &word.segments {
            let (value, field_split) = match segment {
                Segment::Literal { value, .. } => (Cow::Borrowed(value.as_str()), false),
                Segment::Expansion { name, quoted } => {
                    let value = if name == "?" {
                        Cow::Owned(status.to_string())
                    } else {
                        Cow::Borrowed(env.get(name).map_or("", String::as_str))
                    };
                    (value, split && !quoted)
                }
            };
            // Charge before copying or scanning. Field storage is admitted as
            // each field is produced, so whitespace cannot amplify argv freely.
            self.charge(value.len())?;
            if !field_split {
                current.push_str(&value);
                keep = true;
                continue;
            }
            for part in value.split_inclusive([' ', '\t', '\n']) {
                let separated = part.ends_with([' ', '\t', '\n']);
                let text = if separated {
                    &part[..part.len() - 1]
                } else {
                    part
                };
                current.push_str(text);
                keep |= !text.is_empty();
                if separated && keep {
                    self.charge(std::mem::size_of::<String>())?;
                    fields.push(std::mem::take(&mut current));
                    keep = false;
                }
            }
        }
        if keep {
            self.charge(std::mem::size_of::<String>())?;
            fields.push(current);
        }
        Some(fields)
    }
}
