use selfie::package::service::SpecService;

use crate::{config::CliConfig, display_manager::DisplayManager, event_processor::EventProcessor};

use super::list::handle_spec_list_event;

pub(crate) async fn handle_search(
    service: &impl SpecService,
    pattern: &str,
    config: &CliConfig,
    display: &DisplayManager,
) -> i32 {
    tracing::debug!("Running spec search command (pattern={pattern:?})");

    display.print_progress(format!("Searching specs for \"{pattern}\"..."));

    let event_stream = service.search(pattern).await;

    let processor = EventProcessor::new(display.clone());
    let result = processor
        .process_events(event_stream, |event| {
            handle_spec_list_event(event, config, display)
        })
        .await;
    result.exit_code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CliConfig;
    use futures::stream;
    use selfie::package::event::PackageEvent;
    use selfie::{
        config::SelfieConfigBuilder,
        package::event::{
            OperationContext, OperationInfo, OperationResult, OperationSuccess, SpecListData,
            SpecListItem, StepCount, metadata::OperationType,
        },
    };
    use std::time::Instant;
    use uuid::Uuid;

    fn test_op_info() -> OperationInfo {
        OperationInfo {
            id: Uuid::new_v4(),
            operation_type: OperationType::SpecSearch,
            package_name: String::new(),
            environment: "test".to_string(),
            context: OperationContext::default(),
            timestamp: Instant::now(),
        }
    }

    fn test_config() -> CliConfig {
        CliConfig::wrap_for_test(SelfieConfigBuilder::default().environment("test").build())
    }

    #[tokio::test]
    async fn test_search_with_results_returns_exit_0() {
        let config = test_config();
        let display = DisplayManager::new(false);

        let item = SpecListItem {
            name: "ripgrep".to_string(),
            description: Some("Fast search tool".to_string()),
            environments: vec!["macos".to_string()],
            git_status: None,
        };

        let events = vec![
            PackageEvent::SpecListItemCompleted {
                operation_info: test_op_info(),
                spec_item: item.clone(),
            },
            PackageEvent::SpecListLoaded {
                operation_info: test_op_info(),
                spec_list: SpecListData {
                    specs: vec![item],
                    invalid_packages: vec![],
                    current_environment: "test".to_string(),
                    package_directory: "/tmp/packages".to_string(),
                    environment_stats: Default::default(),
                    show_all: true,
                },
            },
            PackageEvent::Completed {
                operation_info: test_op_info(),
                result: OperationResult::Success(OperationSuccess::spec_list_generated(
                    1,
                    0,
                    "test".to_string(),
                    StepCount::new(2, 2),
                )),
            },
        ];

        let event_stream = Box::pin(stream::iter(events));
        let processor = EventProcessor::new(display.clone());
        let result = processor
            .process_events(event_stream, |event| {
                handle_spec_list_event(event, &config, &display)
            })
            .await;

        assert_eq!(result.exit_code, 0);
        assert!(!result.had_errors);
    }

    #[tokio::test]
    async fn test_search_no_results_returns_exit_0() {
        let config = test_config();
        let display = DisplayManager::new(false);

        let events = vec![
            PackageEvent::SpecListLoaded {
                operation_info: test_op_info(),
                spec_list: SpecListData {
                    specs: vec![],
                    invalid_packages: vec![],
                    current_environment: "test".to_string(),
                    package_directory: "/tmp/packages".to_string(),
                    environment_stats: Default::default(),
                    show_all: true,
                },
            },
            PackageEvent::Completed {
                operation_info: test_op_info(),
                result: OperationResult::Success(OperationSuccess::spec_list_generated(
                    0,
                    0,
                    "test".to_string(),
                    StepCount::new(2, 2),
                )),
            },
        ];

        let event_stream = Box::pin(stream::iter(events));
        let processor = EventProcessor::new(display.clone());
        let result = processor
            .process_events(event_stream, |event| {
                handle_spec_list_event(event, &config, &display)
            })
            .await;

        assert_eq!(result.exit_code, 0);
        assert!(!result.had_errors);
    }

    #[tokio::test]
    async fn test_search_failure_returns_exit_1() {
        let config = test_config();
        let display = DisplayManager::new(false);

        let events = vec![PackageEvent::Completed {
            operation_info: test_op_info(),
            result: OperationResult::Failure(selfie::package::event::OperationFailure::Generic(
                "package directory not found".to_string(),
            )),
        }];

        let event_stream = Box::pin(stream::iter(events));
        let processor = EventProcessor::new(display.clone());
        let result = processor
            .process_events(event_stream, |event| {
                handle_spec_list_event(event, &config, &display)
            })
            .await;

        assert_eq!(result.exit_code, 1);
        assert!(result.had_errors);
    }

    #[test]
    fn test_search_event_handler_consumes_spec_list_events() {
        let config = test_config();
        let display = DisplayManager::new(false);

        // SpecListItemCompleted should be consumed
        let item_event = PackageEvent::SpecListItemCompleted {
            operation_info: test_op_info(),
            spec_item: SpecListItem {
                name: "node".to_string(),
                description: Some("JavaScript runtime".to_string()),
                environments: vec!["macos".to_string()],
                git_status: None,
            },
        };
        assert!(handle_spec_list_event(&item_event, &config, &display));

        // SpecListLoaded should be consumed
        let loaded_event = PackageEvent::SpecListLoaded {
            operation_info: test_op_info(),
            spec_list: SpecListData {
                specs: vec![],
                invalid_packages: vec![],
                current_environment: "test".to_string(),
                package_directory: "/tmp/packages".to_string(),
                environment_stats: Default::default(),
                show_all: true,
            },
        };
        assert!(handle_spec_list_event(&loaded_event, &config, &display));

        // Debug events should not be consumed (deferred to default handler)
        let debug_event = PackageEvent::Debug {
            operation_info: test_op_info(),
            message: "some debug".to_string(),
        };
        assert!(!handle_spec_list_event(&debug_event, &config, &display));
    }
}
