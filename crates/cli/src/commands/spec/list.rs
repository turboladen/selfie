use comfy_table::{ContentArrangement, Table, modifiers, presets};
use console::style;
use selfie::package::{
    event::{PackageEvent, SpecListData},
    service::SpecService,
};

use crate::{
    commands::common::format_environment_names, config::CliConfig, display_manager::DisplayManager,
    event_processor::EventProcessor,
};

pub(crate) async fn handle_list(
    service: &impl SpecService,
    config: &CliConfig,
    display: &DisplayManager,
    show_all: bool,
) -> i32 {
    tracing::debug!("Running spec list command (show_all={show_all})");

    display.print_progress("Loading package definitions...");

    let event_stream = service.list(show_all).await;

    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| handle_spec_list_event(event, config))
        .await;
    result.exit_code
}

fn handle_spec_list_event(event: &PackageEvent, config: &CliConfig) -> bool {
    match event {
        PackageEvent::SpecListLoaded { spec_list, .. } => {
            display_spec_list(spec_list, config);
            true
        }
        PackageEvent::SpecListItemCompleted { .. } => {
            true // Consumed — we display everything in the summary
        }
        PackageEvent::Progress { .. } => {
            true // Suppress progress for this fast operation
        }
        _ => false,
    }
}

fn display_spec_list(data: &SpecListData, config: &CliConfig) {
    let use_colors = config.use_colors();

    if data.specs.is_empty() && data.invalid_packages.is_empty() {
        println!("No package definitions found.");
        return;
    }

    if !data.specs.is_empty() {
        let mut table = Table::new();
        table
            .load_preset(presets::UTF8_FULL_CONDENSED)
            .apply_modifier(modifiers::UTF8_ROUND_CORNERS)
            .set_content_arrangement(ContentArrangement::Dynamic);
        table.set_header(vec!["Name", "Version", "Description", "Environments"]);

        for spec in &data.specs {
            table.add_row(vec![
                format_name(&spec.name, use_colors),
                spec.version.clone(),
                spec.description.clone().unwrap_or_default(),
                format_environment_names(&spec.environments, &data.current_environment, config),
            ]);
        }

        println!("{table}");
    }

    // Show invalid packages
    for invalid in &data.invalid_packages {
        let msg = if use_colors {
            format!(
                "  {} {}",
                style("⚠").yellow(),
                style(format!("Invalid: {} — {}", invalid.path, invalid.error)).dim()
            )
        } else {
            format!("  Invalid: {} — {}", invalid.path, invalid.error)
        };
        println!("{msg}");
    }

    // Summary
    let summary = if data.show_all {
        format!("{} spec(s) across all environments", data.specs.len())
    } else {
        format!(
            "{} spec(s) in environment '{}'",
            data.specs.len(),
            data.current_environment,
        )
    };
    println!("\n{summary}");
}

fn format_name(name: &str, use_colors: bool) -> String {
    if use_colors {
        style(name).bold().to_string()
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
                version: "20.0.0".to_string(),
                description: Some("Node.js runtime".to_string()),
                environments: vec!["macos".to_string()],
            },
        };
        assert!(handle_spec_list_event(&event, &config));
    }

    #[test]
    fn test_handle_spec_list_event_consumes_loaded() {
        let config = test_config();
        let event = PackageEvent::SpecListLoaded {
            operation_info: test_op_info(),
            spec_list: SpecListData {
                specs: vec![],
                invalid_packages: vec![],
                current_environment: "test".to_string(),
                package_directory: "/tmp/packages".to_string(),
                environment_stats: Default::default(),
                show_all: false,
            },
        };
        assert!(handle_spec_list_event(&event, &config));
    }

    #[test]
    fn test_handle_spec_list_event_defers_other() {
        let config = test_config();
        let event = PackageEvent::Debug {
            operation_info: test_op_info(),
            message: "some debug".to_string(),
        };
        assert!(!handle_spec_list_event(&event, &config));
    }
}
