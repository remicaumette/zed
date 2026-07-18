use crate::{ConnectionState, DatabasePanel};
use anyhow::{Context as _, Result};
use database::{
    ConnectionProfile, ConnectionProfileError, ConnectionTestResult, DatabaseDriver,
    test_connection,
};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    PathPromptOptions, Render, Task, WeakEntity, Window, rems,
};
use ui::{
    Banner, Button, ButtonStyle, Color, ContextMenu, DropdownMenu, DropdownStyle, Icon, IconName,
    IconPosition, Label, LabelSize, Modal, ModalFooter, ModalHeader, Section, Severity, Switch,
    SwitchLabelPosition, ToggleState, prelude::*,
};
use ui_input::InputField;
use workspace::ModalView;

pub(crate) struct ConnectionEditorModal {
    panel: WeakEntity<DatabasePanel>,
    profile: ConnectionProfile,
    is_new: bool,
    name: Entity<InputField>,
    jdbc_url: Entity<InputField>,
    username: Entity<InputField>,
    password: Entity<InputField>,
    custom_driver_path: Option<Entity<InputField>>,
    status: EditorStatus,
    task: Option<Task<()>>,
}

#[derive(Clone)]
enum EditorStatus {
    Idle,
    Testing,
    DownloadingDriver,
    Saving,
    Connected(ConnectionTestResult),
    Error(String),
}

impl ConnectionEditorModal {
    pub(crate) fn new(
        panel: WeakEntity<DatabasePanel>,
        profile: ConnectionProfile,
        is_new: bool,
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
                .tab_index(2);
            input.set_text(&profile.jdbc_url, window, cx);
            input
        });
        let username = cx.new(|cx| {
            let input = InputField::new(window, cx, "database user")
                .label("Username")
                .tab_index(4);
            if let Some(username) = &profile.username {
                input.set_text(username, window, cx);
            }
            input
        });
        let password = cx.new(|cx| {
            InputField::new(window, cx, "Leave empty to keep the saved password")
                .label("Password")
                .tab_index(5)
                .masked(true)
        });
        let custom_driver_path = (profile.driver == database::DatabaseDriver::Custom).then(|| {
            cx.new(|cx| {
                let input = InputField::new(window, cx, "/path/to/jdbc-driver.jar")
                    .label("JDBC driver JAR")
                    .tab_index(3);
                if let Some(path) = &profile.custom_driver_path {
                    input.set_text(&path.to_string_lossy(), window, cx);
                }
                input
            })
        });

        Self {
            panel,
            profile,
            is_new,
            name,
            jdbc_url,
            username,
            password,
            custom_driver_path,
            status: EditorStatus::Idle,
            task: None,
        }
    }

    fn select_driver(
        &mut self,
        driver: DatabaseDriver,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous_driver = self.profile.driver;
        if driver == previous_driver {
            return;
        }

        let current_url = self.jdbc_url.read(cx).text(cx);
        if current_url.trim() == previous_driver.default_jdbc_url() {
            self.jdbc_url.update(cx, |input, cx| {
                input.set_text(driver.default_jdbc_url(), window, cx)
            });
        }

        let current_name = self.name.read(cx).text(cx);
        if self.is_new && current_name == self.profile.name {
            let previous_base_name = format!("Local {previous_driver}");
            let suffix = self
                .profile
                .name
                .strip_prefix(&previous_base_name)
                .unwrap_or_default();
            let suggested_name = format!("Local {driver}{suffix}");
            self.name
                .update(cx, |input, cx| input.set_text(&suggested_name, window, cx));
            self.profile.name = suggested_name;
        }

        self.profile.driver = driver;
        let custom_driver_path = self.profile.custom_driver_path.clone();
        self.custom_driver_path = (driver == DatabaseDriver::Custom).then(|| {
            cx.new(move |cx| {
                let input = InputField::new(window, cx, "/path/to/jdbc-driver.jar")
                    .label("JDBC driver JAR")
                    .tab_index(3);
                if let Some(path) = &custom_driver_path {
                    input.set_text(&path.to_string_lossy(), window, cx);
                }
                input
            })
        });
        if driver != DatabaseDriver::Custom {
            self.profile.custom_driver_path = None;
        }
        self.status = EditorStatus::Idle;
        cx.notify();
    }

    fn render_driver_selector(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let selected_driver = self.profile.driver;
        let this = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            for driver in DatabaseDriver::ALL {
                let this = this.clone();
                menu = menu.toggleable_entry(
                    driver.display_name(),
                    driver == selected_driver,
                    IconPosition::Start,
                    None,
                    move |window, cx| {
                        this.update(cx, |this, cx| this.select_driver(driver, window, cx))
                            .ok();
                    },
                );
            }
            menu
        });

        v_flex()
            .gap_1()
            .child(Label::new("Driver").size(LabelSize::Small))
            .child(
                DropdownMenu::new(
                    "database-driver-selector",
                    selected_driver.display_name(),
                    menu,
                )
                .style(DropdownStyle::Outlined)
                .full_width(true)
                .tab_index(1),
            )
    }

    fn profile_from_fields(&mut self, cx: &mut Context<Self>) -> Option<ConnectionProfile> {
        self.name
            .update(cx, |input, cx| input.set_error(None::<String>, cx));
        self.jdbc_url
            .update(cx, |input, cx| input.set_error(None::<String>, cx));
        if let Some(input) = &self.custom_driver_path {
            input.update(cx, |input, cx| input.set_error(None::<String>, cx));
        }

        let mut profile = self.profile.clone();
        profile.name = self.name.read(cx).text(cx).trim().to_owned();
        profile.jdbc_url = self.jdbc_url.read(cx).text(cx).trim().to_owned();
        profile.username = match self.username.read(cx).text(cx).trim() {
            "" => None,
            username => Some(username.to_owned()),
        };
        profile.custom_driver_path = self.custom_driver_path.as_ref().and_then(|input| {
            let path = input.read(cx).text(cx);
            (!path.trim().is_empty()).then(|| path.trim().into())
        });

        match profile.validate() {
            Ok(()) => Some(profile),
            Err(ConnectionProfileError::MissingName) => {
                self.name.update(cx, |input, cx| {
                    input.set_error(Some("A connection name is required"), cx)
                });
                None
            }
            Err(ConnectionProfileError::InvalidJdbcUrl) => {
                self.jdbc_url.update(cx, |input, cx| {
                    input.set_error(Some("Expected a URL starting with `jdbc:`"), cx)
                });
                None
            }
            Err(
                ConnectionProfileError::MissingCustomDriverPath
                | ConnectionProfileError::CustomDriverNotFound(_),
            ) => {
                if let Some(input) = &self.custom_driver_path {
                    input.update(cx, |input, cx| {
                        input.set_error(Some("Select an existing JDBC driver JAR"), cx)
                    });
                }
                None
            }
        }
    }

    fn download_driver(&mut self, cx: &mut Context<Self>) {
        let driver = self.profile.driver;
        let http_client = cx.http_client();
        self.status = EditorStatus::DownloadingDriver;
        cx.notify();

        self.task = Some(cx.spawn(async move |this, cx| {
            let result = database::download_jdbc_driver(driver, http_client.as_ref()).await;
            this.update(cx, |this, cx| {
                this.status = match result {
                    Ok(_) => EditorStatus::Idle,
                    Err(error) => EditorStatus::Error(format!(
                        "Could not download the JDBC driver: {error:#}"
                    )),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    fn choose_custom_driver(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Select JDBC driver".into()),
        });

        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = prompt.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                if let Some(input) = &this.custom_driver_path {
                    input.update(cx, |input, cx| {
                        input.set_text(&path.to_string_lossy(), window, cx);
                        input.set_error(None::<String>, cx);
                    });
                }
                this.status = EditorStatus::Idle;
                cx.notify();
            })
            .ok();
        }));
    }

    fn render_driver_setup(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        if let Some(download) = self.profile.driver.download() {
            let installed = database::installed_jdbc_driver_path(self.profile.driver).is_some();
            let is_downloading = matches!(self.status, EditorStatus::DownloadingDriver);
            return Banner::new()
                .severity(if installed {
                    Severity::Success
                } else {
                    Severity::Warning
                })
                .child(Label::new(if installed {
                    format!("JDBC driver {} is installed", download.version)
                } else {
                    format!("JDBC driver {} is not installed", download.version)
                }))
                .when(!installed, |banner| {
                    banner.action_slot(
                        Button::new(
                            "download-jdbc-driver",
                            if is_downloading {
                                "Downloading…"
                            } else {
                                "Download Driver"
                            },
                        )
                        .disabled(is_downloading)
                        .on_click(cx.listener(|this, _, _, cx| this.download_driver(cx))),
                    )
                })
                .into_any_element();
        }

        let driver_path = self
            .custom_driver_path
            .clone()
            .expect("custom connections have a driver path field");
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .items_end()
                    .gap_2()
                    .flex_1()
                    .child(div().flex_1().child(driver_path))
                    .child(
                        Button::new("browse-custom-jdbc-driver", "Browse…")
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.choose_custom_driver(window, cx)
                            })),
                    ),
            )
            .child(
                Label::new("Select the JDBC driver JAR or a self-contained driver JAR.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
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

    fn render_status(&self) -> Option<impl IntoElement + use<>> {
        match &self.status {
            EditorStatus::Idle => None,
            EditorStatus::Testing => Some(
                Banner::new()
                    .severity(Severity::Info)
                    .child(Label::new("Testing the JDBC connection…"))
                    .into_any_element(),
            ),
            EditorStatus::DownloadingDriver => Some(
                Banner::new()
                    .severity(Severity::Info)
                    .child(Label::new("Downloading the JDBC driver…"))
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_busy = matches!(
            self.status,
            EditorStatus::Testing | EditorStatus::DownloadingDriver | EditorStatus::Saving
        );
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
                            .headline(if self.is_new {
                                "New connection".to_owned()
                            } else {
                                format!("Edit {} connection", self.profile.driver)
                            })
                            .description("Configure a JDBC connection. Secrets are stored separately from the profile."),
                    )
                    .section(
                        Section::new().child(
                            v_flex()
                                .gap_3()
                                .child(self.name.clone())
                                .child(self.render_driver_selector(window, cx))
                                .child(self.render_driver_setup(cx))
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
                                        .child(
                                            Switch::new("database-read-only", read_only.into())
                                                .label("Read-only connection")
                                                .label_position(SwitchLabelPosition::Start)
                                                .full_width(true)
                                                .tab_index(6_isize)
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
                                        .child(
                                            Label::new(
                                                "Ask the JDBC driver to reject write operations.",
                                            )
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                        ),
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
