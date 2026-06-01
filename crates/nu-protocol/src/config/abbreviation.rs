use super::{config_update_string_enum, prelude::*};
use std::collections::HashMap;

/// Controls where in the command line an abbreviation is allowed to expand.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AbbrPosition {
    /// Expand only when the token is at command position.
    ///
    /// Note: `sudo`, `env`, and `noglob` are **not** transparent decorators;
    /// a token following them is at argument position and will not expand.
    #[default]
    Command,
    /// Expand anywhere in the line, regardless of position.
    Anywhere,
}

impl IntoValue for AbbrPosition {
    fn into_value(self, span: Span) -> Value {
        match self {
            AbbrPosition::Command => Value::string("command", span),
            AbbrPosition::Anywhere => Value::string("anywhere", span),
        }
    }
}

impl FromStr for AbbrPosition {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "command" => Ok(AbbrPosition::Command),
            "anywhere" => Ok(AbbrPosition::Anywhere),
            _ => Err("expected 'command' or 'anywhere'"),
        }
    }
}

impl UpdateFromValue for AbbrPosition {
    fn update(&mut self, value: &Value, path: &mut ConfigPath, errors: &mut ConfigErrors) {
        config_update_string_enum(self, value, path, errors)
    }
}

/// A single abbreviation definition.
///
/// In `$env.config.abbreviations`, each value may be either:
/// - A plain string — compatibility shorthand for `{expansion: "...", position: "anywhere"}`
/// - A record with these fields:
///
/// | field | type | default | description |
/// |---|---|---|
/// | `expansion` | string | **required** | Text to substitute on trigger |
/// | `position` | `"command"` or `"anywhere"` | `"command"` | Expand only at command position, or everywhere |
/// | `cursor_marker` | string or null | null | String in `expansion` marking cursor placement; when null no cursor placement occurs |
///
/// When `cursor_marker` is set, the first occurrence of that string in `expansion` is
/// removed and the cursor is placed there.  Can be a single character like `"%"` or a
/// multi-character sentinel like `"--cursor--"`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbbreviationDef {
    /// The text that replaces the matched token.
    pub expansion: String,
    /// Whether to expand only at command position or anywhere.
    pub position: AbbrPosition,
    /// Optional cursor-placement marker string.
    /// The first occurrence of this string in `expansion` is removed and the
    /// cursor placed there.  May be multi-character, e.g. `"--cursor--"`.
    pub cursor_marker: Option<String>,
}

impl AbbreviationDef {
    fn shorthand(expansion: String) -> Self {
        Self {
            expansion,
            position: AbbrPosition::Anywhere,
            ..Default::default()
        }
    }

    fn from_config_value<'a>(
        value: &'a Value,
        path: &mut ConfigPath<'a>,
        errors: &mut ConfigErrors,
    ) -> Option<Self> {
        match value {
            Value::String { val, .. } => Some(Self::shorthand(val.clone())),
            Value::Record { val: record, .. } => {
                let mut def = Self::default();
                let mut has_expansion = false;
                let mut valid = true;

                for (field, field_value) in record.iter() {
                    let field_path = &mut path.push(field);
                    match field.as_str() {
                        "expansion" => match field_value {
                            Value::String { val, .. } => {
                                def.expansion = val.clone();
                                has_expansion = true;
                            }
                            _ => {
                                errors.type_mismatch(field_path, Type::String, field_value);
                                valid = false;
                            }
                        },
                        "position" => match field_value {
                            Value::String { val, .. } => match val.parse() {
                                Ok(position) => {
                                    def.position = position;
                                }
                                Err(err) => {
                                    errors.invalid_value(field_path, err, field_value);
                                    valid = false;
                                }
                            },
                            _ => {
                                errors.type_mismatch(field_path, Type::String, field_value);
                                valid = false;
                            }
                        },
                        "cursor_marker" => match field_value {
                            Value::String { val, .. } if val.is_empty() => {
                                errors.invalid_value(
                                    field_path,
                                    "a non-empty cursor marker or null",
                                    field_value,
                                );
                                valid = false;
                            }
                            Value::String { val, .. } => def.cursor_marker = Some(val.clone()),
                            Value::Nothing { .. } => def.cursor_marker = None,
                            _ => {
                                errors.type_mismatch(field_path, Type::String, field_value);
                                valid = false;
                            }
                        },
                        _ => {
                            errors.unknown_option(field_path, field_value);
                            valid = false;
                        }
                    }
                }

                if !has_expansion {
                    errors.missing_column(path, "expansion", value.span());
                    valid = false;
                }

                valid.then_some(def)
            }
            _ => {
                errors.type_mismatch(path, Type::String, value);
                None
            }
        }
    }
}

pub(super) fn update_abbreviations_from_value<'a>(
    abbreviations: &mut HashMap<String, AbbreviationDef>,
    value: &'a Value,
    path: &mut ConfigPath<'a>,
    errors: &mut ConfigErrors,
) {
    let Value::Record { val: record, .. } = value else {
        errors.type_mismatch(path, Type::record(), value);
        return;
    };

    let mut parsed = HashMap::new();
    for (key, value) in record.iter() {
        let path = &mut path.push(key);
        if let Some(def) = AbbreviationDef::from_config_value(value, path, errors) {
            parsed.insert(key.clone(), def);
        }
    }
    *abbreviations = parsed;
}

impl IntoValue for AbbreviationDef {
    fn into_value(self, span: Span) -> Value {
        if self.position == AbbrPosition::Anywhere && self.cursor_marker.is_none() {
            return Value::string(self.expansion, span);
        }

        record! {
            "expansion"     => Value::string(self.expansion, span),
            "position"      => self.position.into_value(span),
            "cursor_marker" => self.cursor_marker
                .map_or_else(|| Value::nothing(span), |s| Value::string(s, span)),
        }
        .into_value(span)
    }
}
