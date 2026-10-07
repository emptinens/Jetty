//! Minimal single-line text fields and the new-session form.

/// One editable line of text.
#[derive(Clone, Default)]
pub struct TextField {
    pub value: String,
}

impl TextField {
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
        }
    }

    /// Apply one key. Returns true when the field consumed it.
    pub fn apply_key(&mut self, key: &str, key_char: Option<&str>, command_modifier: bool) -> bool {
        if command_modifier {
            return false;
        }
        match key {
            "backspace" => {
                self.value.pop();
                true
            }
            "enter" | "escape" | "tab" => false,
            "space" => {
                self.value.push(' ');
                true
            }
            _ => match key_char {
                Some(text) if !text.is_empty() && !text.chars().any(char::is_control) => {
                    self.value.push_str(text);
                    true
                }
                _ => false,
            },
        }
    }
}

/// What the form wants after a key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FormOutcome {
    Consumed,
    Next,
    Cancel,
    Submit,
}

/// Three fields: name, directory, command.
pub struct NewSessionForm {
    pub fields: [TextField; 3],
    pub active: usize,
}

pub const FIELD_LABELS: [&str; 3] = ["name", "directory", "command"];

impl NewSessionForm {
    pub fn with_defaults(directory: String, command: String) -> Self {
        Self {
            fields: [
                TextField::new(""),
                TextField::new(directory),
                TextField::new(command),
            ],
            active: 0,
        }
    }

    /// (name, directory, command)
    pub fn values(&self) -> (String, String, String) {
        (
            self.fields[0].value.clone(),
            self.fields[1].value.clone(),
            self.fields[2].value.clone(),
        )
    }

    pub fn handle_key(
        &mut self,
        key: &str,
        key_char: Option<&str>,
        command_modifier: bool,
    ) -> FormOutcome {
        match key {
            "escape" => return FormOutcome::Cancel,
            "enter" => {
                if self.active + 1 < self.fields.len() {
                    self.active += 1;
                    return FormOutcome::Next;
                }
                return FormOutcome::Submit;
            }
            "tab" => {
                self.active = (self.active + 1) % self.fields.len();
                return FormOutcome::Next;
            }
            _ => {}
        }
        self.fields[self.active].apply_key(key, key_char, command_modifier);
        FormOutcome::Consumed
    }

    pub fn handle_event(&mut self, event: &gpui::KeyDownEvent) -> FormOutcome {
        let keystroke = &event.keystroke;
        let command_modifier = keystroke.modifiers.control || keystroke.modifiers.platform;
        self.handle_key(
            &keystroke.key,
            keystroke.key_char.as_deref(),
            command_modifier,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_takes_text_and_backspace() {
        let mut field = TextField::new("");
        assert!(field.apply_key("h", Some("h"), false));
        assert!(field.apply_key("i", Some("i"), false));
        assert!(field.apply_key("space", Some(" "), false));
        assert!(field.apply_key("backspace", None, false));
        assert_eq!(field.value, "hi");
        assert!(!field.apply_key("a", Some("a"), true));
        assert_eq!(field.value, "hi");
    }

    #[test]
    fn form_enter_advances_then_submits_and_escape_cancels() {
        let mut form = NewSessionForm::with_defaults("/home/x".into(), "/bin/bash".into());
        assert_eq!(form.handle_key("enter", None, false), FormOutcome::Next);
        assert_eq!(form.active, 1);
        assert_eq!(form.handle_key("enter", None, false), FormOutcome::Next);
        assert_eq!(form.active, 2);
        assert_eq!(form.handle_key("enter", None, false), FormOutcome::Submit);
        assert_eq!(form.handle_key("escape", None, false), FormOutcome::Cancel);

        form.active = 0;
        form.handle_key("x", Some("x"), false);
        assert_eq!(form.values().0, "x");
        assert_eq!(form.values().1, "/home/x");
    }
}