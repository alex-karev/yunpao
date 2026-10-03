use anyhow::Result;
use cliclack::{input, select};
use yunpao::storage::{ProjectState, Session};

/// Select session dialog
pub fn select_session_dialog<'a>(
    state: &'a ProjectState,
    query: Option<String>,
) -> Result<&'a Session> {
    let id = if let Some(v) = query {
        v
    } else {
        let mut session_dialog = select("Choose session");
        for session in state.list_sessions() {
            session_dialog = session_dialog.item(
                session.id.clone(),
                session.name.clone(),
                format!("[{}]", session.created_at_str()),
            );
        }
        if let Some(current_session) = state.current_session() {
            session_dialog = session_dialog.initial_value(current_session.id.clone());
        }
        session_dialog.interact()?
    };
    let session = state.find_session(&id)?;
    Ok(session)
}

/// New session name dialog
pub fn new_session_name_dialog(
    name: Option<String>,
    non_interactive: bool,
) -> Result<Option<String>> {
    let name = if !non_interactive && name.is_none() {
        let new_name: String = input("Enter new session name")
            .default_input("")
            .placeholder("Default: current datetime")
            .interact()?;
        if !new_name.is_empty() {
            Some(new_name)
        } else {
            None
        }
    } else {
        None
    };
    Ok(name)
}
