use crate::{ConnectionState, DatabasePanel};
use anyhow::{Context as _, Result};
use database::{
    ConnectionEnvironment, ConnectionProfile, ConnectionProfileError, ConnectionTestResult,
    test_connection,
};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    Task, WeakEntity, Window, rems,
};
use ui::{
    Banner, Button, ButtonStyle, Color, Icon, IconName, Label, LabelSize, Modal, ModalFooter,
    ModalHeader, Section, Severity, Switch, ToggleState, prelude::*,
};
use ui_input::InputField;
use workspace::ModalView;

pub(crate) struct ConnectionEditorModal {
    panel: WeakEntity<DatabasePanel>,
    profile: ConnectionProfile,
    name: Entity<InputField>,
    jdbc_url: Entity<InputField>,
    username: Entity<InputField>,
    password: Entity<InputField>,
    status: EditorStatus,
    task: Option<Task<()>>,
}

#[derive(Clone)]
enum EditorStatus {
    Idle,
    Testing,
    Saving,
    Connected(ConnectionTestResult),
    Error(String),
}

impl ConnectionEditorModal {
    pub(crate) fn new(
        panel: WeakEntity<DatabasePanel>,
        profile: ConnectionProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| {
            let input = InputField::new(window, cx, "My database")
                .label("Name")
                .tab_index(0);
            input.set_text(&profile.name, window, cx);
            input
        });
        let jdbc_url = cx.new(|cx| {
            let input = InputField::new(window, cx, profile.driver.default_jdbc_url())
                .label("JDBC URL")
                .tab_index(1);
            input.set_text(&profile.jdbc_url, window, cx);
            input
        });
        let username = cx.new(|cx| {
            let input = InputField::new(window, cx, "database user")
                .label("Username")
                .tab_index(2);
            if let Some(username) = &profile.username {
                input.set_text(username, window, cx);
            }
            input
        });
        let password = cx.new(|cx| {
            InputField::new(window, cx, "Leave empty to keep the saved password")
                .label("Password")
                .tab_index(3)
                .masked(true)
        });

        Self {
            panel,
            profile,
            name,
            jdbc_url,
            username,
            password,
            status: EditorStatus::Idle,
            task: None,
        }
    }

    fn profile_from_fields(&mut self, cx: &mut Context<Self>) -> Option<ConnectionProfile> {
        self.name
            .update(cx, |input, cx| input.set_error(None::<String>, cx));
        self.jdbc_url
            .update(cx, |input, cx| input.set_error(None::<String>, cx));

        let mut profile = self.profile.clone();
        profile.name = self.name.read(cx).text(cx).trim().to_owned();
        profile.jdbc_url = self.jdbc_url.read(cx).text(cx).trim().to_owned();
        profile.username = match self.username.read(cx).text(cx).trim() {
            "" => None,
            username => Some(username.to_owned()),
        };

        match profile.validate() {
            Ok(()) => Some(profile),
            Err(ConnectionProfileError::MissingName) => {
                self.name.update(cx, |input, cx| {
                    input.set_error(Some("A connection name is required"), cx)
                });
                None
            }
            Err(
                ConnectionProfileError::InvalidJdbcUrl
                | ConnectionProfileError::DriverUrlMismatch(_),
            ) => {
                let message = format!(
                    "Expected a URL starting with `{}`",
                    profile.driver.jdbc_url_prefix()
                );
                self.jdbc_url
                    .update(cx, |input, cx| input.set_error(Some(message), cx));
                None
            }
        }
    }

    async fn password_for_test(
        profile: &ConnectionProfile,
        typed_password: String,
        credentials_provider: &dyn credentials_provider::CredentialsProvider,
        cx: &gpui::AsyncApp,
    ) -> Result<Option<String>> {
        if !typed_password.is_empty() {
            return Ok(Some(typed_password));
        }

        credentials_provider
            .read_credentials(&profile.id.credential_key(), cx)
            .await?
            .map(|(_, password)| {
                String::from_utf8(password).context("saved database password is not valid UTF-8")
            })
            .transpose()
    }

    fn test(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_from_fields(cx) else {
            return;
        };
        let typed_password = self.password.read(cx).text(cx);
        let credentials_provider = zed_credentials_provider::global(cx);
        let panel = self.panel.clone();
        let connection_id = profile.id;
        self.status = EditorStatus::Testing;
        cx.notify();

        self.task = Some(cx.spawn(async move |this, cx| {
            let result = async {
                let password = Self::password_for_test(
                    &profile,
                    typed_password,
                    credentials_provider.as_ref(),
                    cx,
                )
                .await?;
                test_connection(&profile, password.as_deref()).await
            }
            .await;

            match result {
                Ok(result) => {
                    panel
                        .update(cx, |panel, cx| {
                            panel.set_connection_state(
                                connection_id,
                                ConnectionState::Connected,
                                cx,
                            )
                        })
                        .ok();
                    this.update(cx, |this, cx| {
                        this.status = EditorStatus::Connected(result);
                        cx.notify();
                    })
                    .ok();
                }
                Err(error) => {
                    let error = error.to_string();
                    panel
                        .update(cx, |panel, cx| {
                            panel.set_connection_state(connection_id, ConnectionState::Error, cx)
                        })
                        .ok();
                    this.update(cx, |this, cx| {
                        this.status = EditorStatus::Error(error);
                        cx.notify();
                    })
                    .ok();
                }
            }
        }));
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profile_from_fields(cx) else {
            return;
        };
        let password = self.password.read(cx).text(cx);
        let credentials_provider = zed_credentials_provider::global(cx);
        let panel = self.panel.clone();
        self.status = EditorStatus::Saving;
        cx.notify();

        self.task = Some(cx.spawn(async move |this, cx| {
            let write_result = if password.is_empty() {
                Ok(())
            } else {
                credentials_provider
                    .write_credentials(
                        &profile.id.credential_key(),
                        profile.username.as_deref().unwrap_or("database"),
                        password.as_bytes(),
                        cx,
                    )
                    .await
            };

            match write_result {
                Ok(()) => {
                    panel
                        .update(cx, |panel, cx| panel.upsert_connection(profile, cx))
                        .ok();
                    this.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
                }
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.status =
                            EditorStatus::Error(format!("Could not save the password: {error:#}"));
                        cx.notify();
                    })
                    .ok();
                }
            }
        }));
    }

    fn set_environment(&mut self, environment: ConnectionEnvironment, cx: &mut Context<Self>) {
        self.profile.environment = environment;
        self.status = EditorStatus::Idle;
        cx.notify();
    }

    fn render_environment_button(
        &self,
        environment: ConnectionEnvironment,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        Button::new(format!("environment-{environment:?}"), label)
            .style(ButtonStyle::OutlinedGhost)
            .toggle_state(self.profile.environment == environment)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_environment(environment, cx);
            }))
    }

    fn render_status(&self) -> Option<impl IntoElement + use<>> {
        match &self.status {
            EditorStatus::Idle => None,
            EditorStatus::Testing => Some(
                Banner::new()
                    .severity(Severity::Info)
                    .child(Label::new("Testing the JDBC connection…"))
                    .into_any_element(),
            ),
            EditorStatus::Saving => Some(
                Banner::new()
                    .severity(Severity::Info)
                    .child(Label::new("Saving the connection…"))
                    .into_any_element(),
            ),
            EditorStatus::Connected(result) => Some(
                Banner::new()
                    .severity(Severity::Success)
                    .child(
                        Label::new(format!(
                            "Connected to {} {} in {} ms with {} {}",
                            result.database_product,
                            result.database_version,
                            result.round_trip_millis,
                            result.driver_name,
                            result.driver_version
                        ))
                        .size(LabelSize::Small),
                    )
                    .into_any_element(),
            ),
            EditorStatus::Error(error) => Some(
                Banner::new()
                    .severity(Severity::Error)
                    .wrap_content(true)
                    .child(Label::new(error.clone()).size(LabelSize::Small))
                    .into_any_element(),
            ),
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl Focusable for ConnectionEditorModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for ConnectionEditorModal {}
impl ModalView for ConnectionEditorModal {}

impl Render for ConnectionEditorModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_busy = matches!(self.status, EditorStatus::Testing | EditorStatus::Saving);
        let read_only = self.profile.read_only;
        let this = cx.weak_entity();

        v_flex()
            .w(rems(38.))
            .max_h(rems(46.))
            .elevation_3(cx)
            .key_context("DatabaseConnectionEditor")
            .on_action(cx.listener(Self::cancel))
            .child(
                Modal::new("database-connection-editor", None)
                    .header(
                        ModalHeader::new()
                            .icon(Icon::new(IconName::DatabaseZap).color(Color::Muted))
                            .headline(format!("{} connection", self.profile.driver))
                            .description("Configure a JDBC connection. Secrets are stored separately from the profile."),
                    )
                    .section(
                        Section::new().child(
                            v_flex()
                                .gap_3()
                                .child(self.name.clone())
                                .child(self.jdbc_url.clone())
                                .child(
                                    Label::new("JDBC options such as SSL can be added to the URL query string.")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(self.username.clone())
                                .child(self.password.clone())
                                .child(
                                    v_flex()
                                        .gap_1()
                                        .child(Label::new("Environment").size(LabelSize::Small))
                                        .child(
                                            h_flex()
                                                .gap_1()
                                                .child(self.render_environment_button(
                                                    ConnectionEnvironment::Local,
                                                    "Local",
                                                    cx,
                                                ))
                                                .child(self.render_environment_button(
                                                    ConnectionEnvironment::Development,
                                                    "Development",
                                                    cx,
                                                ))
                                                .child(self.render_environment_button(
                                                    ConnectionEnvironment::Staging,
                                                    "Staging",
                                                    cx,
                                                ))
                                                .child(self.render_environment_button(
                                                    ConnectionEnvironment::Production,
                                                    "Production",
                                                    cx,
                                                )),
                                        ),
                                )
                                .child(
                                    Switch::new("database-read-only", read_only.into())
                                        .label("Open connections in read-only mode")
                                        .tab_index(4_isize)
                                        .on_click(move |state, _, cx| {
                                            this.update(cx, |this, cx| {
                                                this.profile.read_only =
                                                    *state == ToggleState::Selected;
                                                this.status = EditorStatus::Idle;
                                                cx.notify();
                                            })
                                            .ok();
                                        }),
                                )
                                .when_some(self.render_status(), |this, status| this.child(status)),
                        ),
                    )
                    .footer(
                        ModalFooter::new()
                            .start_slot(
                                Button::new("test-database-connection", "Test Connection")
                                    .style(ButtonStyle::Outlined)
                                    .disabled(is_busy)
                                    .on_click(cx.listener(|this, _, _, cx| this.test(cx))),
                            )
                            .end_slot(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Button::new("cancel-database-connection", "Cancel")
                                            .disabled(is_busy)
                                            .on_click(cx.listener(|_, _, _, cx| {
                                                cx.emit(DismissEvent)
                                            })),
                                    )
                                    .child(
                                        Button::new("save-database-connection", "Save")
                                            .style(ButtonStyle::Filled)
                                            .disabled(is_busy)
                                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                                    ),
                            ),
                    ),
            )
    }
}
