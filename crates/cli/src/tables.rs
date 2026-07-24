use comfy_table::{
    ContentArrangement, Table, modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL_CONDENSED,
};
use console::style;
use selfie::validation::ValidationIssue;

pub(crate) struct ValidationTableReporter {
    table: Table,
    use_colors: bool,
}

impl ValidationTableReporter {
    pub(crate) fn new(use_colors: bool) -> Self {
        Self {
            table: Table::new(),
            use_colors,
        }
    }

    pub(crate) fn setup(&mut self, header: Vec<&'static str>) -> &mut Self {
        self.table
            .load_preset(UTF8_FULL_CONDENSED)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_header(header);

        self
    }

    pub(crate) fn add_validation_errors(&mut self, error_issues: &[&ValidationIssue]) -> &mut Self {
        for error in error_issues {
            let category = if self.use_colors {
                style(error.category().to_string())
                    .for_stderr()
                    .red()
                    .bold()
                    .to_string()
            } else {
                error.category().to_string()
            };
            self.table.add_row(vec![
                category,
                error.field().to_string(),
                error.message().to_string(),
                error
                    .suggestion()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
            ]);
        }

        self
    }

    pub(crate) fn add_validation_warnings(
        &mut self,
        warning_issues: &[&ValidationIssue],
    ) -> &mut Self {
        for warning in warning_issues {
            let category = if self.use_colors {
                style(warning.category().to_string())
                    .for_stderr()
                    .yellow()
                    .bold()
                    .to_string()
            } else {
                warning.category().to_string()
            };
            self.table.add_row(vec![
                category,
                warning.field().to_string(),
                warning.message().to_string(),
                warning
                    .suggestion()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
            ]);
        }

        self
    }

    pub(crate) fn print(&self) {
        eprintln!("{}", self.table);
    }
}
