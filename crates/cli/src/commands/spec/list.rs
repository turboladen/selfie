use crate::tables::{Cell, SafeTable};
use crate::terminal_text::DisplayPath;
use crate::terminal_text::{Shown, shown};
use selfie::package::{
    event::{PackageEvent, SpecListData},
    service::SpecService,
};

use crate::{
    commands::presentation::format_environment_names,
    config::CliConfig,
    display_manager::DisplayManager,
    event_processor::{EventProcessor, Exit},
    git_style,
};

pub(crate) async fn handle_list(
    service: &impl SpecService,
    config: &CliConfig,
    display: &DisplayManager,
    show_all: bool,
) -> Exit {
    tracing::debug!("Running spec list command (show_all={show_all})");

    display.print_status("Loading specs...");

    let event_stream = service.list(show_all).await;

    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| {
            handle_spec_list_event(event, config, display)
        })
        .await;
    result.exit
}

pub(super) fn handle_spec_list_event(
    event: &PackageEvent,
    config: &CliConfig,
    display: &DisplayManager,
) -> bool {
    match event {
        PackageEvent::SpecListLoaded { spec_list, .. } => {
            display_spec_list(spec_list, config, display);
            true
        }
        PackageEvent::SpecListItemCompleted { .. } => {
            true // Consumed — we display everything in the summary
        }
        _ => false,
    }
}

fn display_spec_list(data: &SpecListData, config: &CliConfig, display: &DisplayManager) {
    let use_colors = config.use_colors();

    if data.specs.is_empty() && data.invalid_packages.is_empty() && data.refused.is_empty() {
        display.print_info("No specs found.");
        return;
    }

    if !data.specs.is_empty() {
        let mut table = SafeTable::new();
        table.set_header(["Name", "Description", "Environments", "Git"]);

        for spec in &data.specs {
            let git_col = spec
                .git_status
                .as_ref()
                .map(|s| git_style::format_git_status_short(s, use_colors))
                .unwrap_or_default();
            table.add_row([
                Cell::painted(&spec.name, use_colors, |s| s.bold()),
                Cell::prose(spec.description.as_deref().unwrap_or_default()),
                format_environment_names(&spec.environments, &data.current_environment, config)
                    .into(),
                git_col.into(),
            ]);
        }

        display.println(shown!("{}", table));
    }

    // Show invalid packages
    for invalid in &data.invalid_packages {
        let text = shown!(
            "Invalid: {} — {}",
            DisplayPath::of(invalid.package_path()),
            Shown::block(&invalid.to_string())
        );
        display.println(issue_line(text, use_colors));
    }

    for refused in &data.refused {
        let text = shown!(
            "Refused: {} ({}) — {}",
            Shown::name(&refused.package_name),
            DisplayPath::of(&refused.path),
            Shown::block(&refused.reason.to_string())
        );
        display.println(issue_line(text, use_colors));
    }

    // Summary
    let summary = if data.show_all {
        shown!("{} spec(s) across all environments", data.specs.len())
    } else {
        shown!(
            "{} spec(s) in environment '{}'",
            data.specs.len(),
            Shown::name(&data.current_environment),
        )
    };
    display.println(shown!("\n{}", summary));
}

// An invalid or refused spec's line under the table. One sentence, styled or
// not, so the two cases cannot drift apart. The lines after its first start
// under its text, past the mark.
fn issue_line(text: Shown, use_colors: bool) -> Shown {
    let text = Shown::indented(text, "    ");
    shown!(
        "  {} {}",
        Shown::from("⚠").paint(use_colors, |s| s.yellow()),
        text.paint(use_colors, |s| s.dim())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // A refusal's continuation lines start under its text, past the mark, and
    // are indented once.
    #[test]
    fn an_issue_line_indents_its_continuation_once() {
        let line = issue_line(shown!("Refused: {}", Shown::block("a\nb")), false);

        assert_eq!(line, "  ⚠ Refused: a\n    b");
    }
    use crate::config::CliConfig;
    use selfie::{
        config::SelfieConfigBuilder,
        package::event::{OperationContext, OperationInfo, SpecListItem, metadata::OperationType},
    };
    use std::time::Instant;
    use uuid::Uuid;

    fn test_op_info() -> OperationInfo {
        OperationInfo {
            id: Uuid::new_v4(),
            operation_type: OperationType::SpecList,
            package_name: String::new(),
            environment: "test".to_string(),
            context: OperationContext::default(),
            timestamp: Instant::now(),
        }
    }

    fn test_config() -> CliConfig {
        CliConfig::wrap_for_test(SelfieConfigBuilder::default().environment("test").build())
    }

    #[test]
    fn test_handle_spec_list_event_consumes_item() {
        let config = test_config();
        let event = PackageEvent::SpecListItemCompleted {
            operation_info: test_op_info(),
            spec_item: SpecListItem {
                name: "node".to_string(),
                description: Some("Node.js runtime".to_string()),
                environments: vec!["macos".to_string()],
                git_status: None,
            },
        };
        let display = DisplayManager::new(false);
        assert!(handle_spec_list_event(&event, &config, &display));
    }

    #[test]
    fn test_handle_spec_list_event_consumes_loaded() {
        let config = test_config();
        let display = DisplayManager::new(false);
        let event = PackageEvent::SpecListLoaded {
            operation_info: test_op_info(),
            spec_list: SpecListData {
                specs: vec![],
                invalid_packages: vec![],
                refused: vec![],
                current_environment: "test".to_string(),
                package_directory: "/tmp/packages".to_string(),
                environment_stats: Default::default(),
                show_all: false,
            },
        };
        assert!(handle_spec_list_event(&event, &config, &display));
    }

    #[test]
    fn test_handle_spec_list_event_defers_other() {
        let config = test_config();
        let display = DisplayManager::new(false);
        let event = PackageEvent::Debug {
            operation_info: test_op_info(),
            message: "some debug".to_string(),
        };
        assert!(!handle_spec_list_event(&event, &config, &display));
    }
}
