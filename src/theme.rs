use clap::ValueEnum;
use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[value(rename_all = "kebab-case")]
pub enum ThemeKind {
    #[default]
    Default,
    Monochrome,
    HighContrast,
}

impl ThemeKind {
    pub fn next(self) -> Self {
        match self {
            Self::Default => Self::Monochrome,
            Self::Monochrome => Self::HighContrast,
            Self::HighContrast => Self::Default,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Monochrome => "monochrome",
            Self::HighContrast => "high-contrast",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub title: Style,
    pub columns: Style,
    pub directory: Style,
    pub symlink: Style,
    pub error: Style,
    pub mountpoint: Style,
    pub selected: Style,
    pub matched: Style,
    pub accent: Style,
    pub dialog: Style,
}

impl Theme {
    pub fn for_kind(kind: ThemeKind) -> Self {
        match kind {
            ThemeKind::Default => Self {
                title: Style::default().add_modifier(Modifier::BOLD),
                columns: Style::default().add_modifier(Modifier::BOLD),
                directory: Style::default().fg(Color::Cyan),
                symlink: Style::default().fg(Color::Magenta),
                error: Style::default().fg(Color::Red),
                mountpoint: Style::default().fg(Color::Yellow),
                selected: Style::default()
                    .fg(Color::Black)
                    .bg(Color::White)
                    .add_modifier(Modifier::BOLD),
                matched: Style::default().fg(Color::Yellow),
                accent: Style::default().fg(Color::Green),
                dialog: Style::default(),
            },
            ThemeKind::Monochrome => Self {
                title: Style::default().add_modifier(Modifier::BOLD),
                columns: Style::default().add_modifier(Modifier::BOLD),
                directory: Style::default().add_modifier(Modifier::BOLD),
                symlink: Style::default().add_modifier(Modifier::ITALIC),
                error: Style::default().add_modifier(Modifier::REVERSED),
                mountpoint: Style::default().add_modifier(Modifier::UNDERLINED),
                selected: Style::default().add_modifier(Modifier::REVERSED),
                matched: Style::default().add_modifier(Modifier::UNDERLINED),
                accent: Style::default().add_modifier(Modifier::BOLD),
                dialog: Style::default(),
            },
            ThemeKind::HighContrast => Self {
                title: Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
                columns: Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
                directory: Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
                symlink: Style::default().fg(Color::LightMagenta),
                error: Style::default().fg(Color::White).bg(Color::Red),
                mountpoint: Style::default().fg(Color::Black).bg(Color::Yellow),
                selected: Style::default()
                    .fg(Color::Black)
                    .bg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
                matched: Style::default().fg(Color::Black).bg(Color::LightYellow),
                accent: Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
                dialog: Style::default().fg(Color::White),
            },
        }
    }
}
