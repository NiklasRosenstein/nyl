use chrono::{DateTime, Utc};
use colored::Colorize;

use crate::cli::table::Cell;
use crate::kubernetes::ReleaseStatus;

/// Format a timestamp as a human-readable age
pub fn format_age(timestamp: &DateTime<Utc>) -> String {
    let now = Utc::now();
    let duration = now.signed_duration_since(*timestamp);

    if duration.num_days() > 0 {
        format!("{}d", duration.num_days())
    } else if duration.num_hours() > 0 {
        format!("{}h", duration.num_hours())
    } else if duration.num_minutes() > 0 {
        format!("{}m", duration.num_minutes())
    } else {
        format!("{}s", duration.num_seconds())
    }
}

/// Format a timestamp as `YYYY-MM-DD HH:MM:SS UTC` string
pub fn format_timestamp(timestamp: &DateTime<Utc>) -> String {
    timestamp.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

/// Color a status string based on the status value
pub fn color_status(status: &ReleaseStatus) -> Cell {
    let text = format!("{:?}", status).to_lowercase();
    let rendered = match status {
        ReleaseStatus::Deployed => text.as_str().green().to_string(),
        ReleaseStatus::Failed => text.as_str().red().to_string(),
        ReleaseStatus::Rendered => text.as_str().yellow().to_string(),
        ReleaseStatus::Superseded => text.as_str().bright_black().to_string(),
    };
    Cell::styled(text, rendered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn test_format_age() {
        let now = Utc::now();

        let two_days_ago = now - Duration::days(2);
        assert_eq!(format_age(&two_days_ago), "2d");

        let five_hours_ago = now - Duration::hours(5);
        assert_eq!(format_age(&five_hours_ago), "5h");

        let thirty_minutes_ago = now - Duration::minutes(30);
        assert_eq!(format_age(&thirty_minutes_ago), "30m");

        let ten_seconds_ago = now - Duration::seconds(10);
        assert_eq!(format_age(&ten_seconds_ago), "10s");
    }

    #[test]
    fn test_format_timestamp() {
        let timestamp = DateTime::parse_from_rfc3339("2024-02-06T14:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let formatted = format_timestamp(&timestamp);
        assert_eq!(formatted, "2024-02-06 14:30:00 UTC");
    }
}
