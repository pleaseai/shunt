/// Validate the complete reset before the legacy parser converts it to Unix time.
pub(super) fn parse(input: &str) -> Option<u64> {
    let bytes = input.as_bytes();
    if !input.is_ascii()
        || bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || [0..4, 5..7, 8..10, 11..13, 14..16, 17..19]
            .into_iter()
            .any(|range| !bytes[range].iter().all(u8::is_ascii_digit))
    {
        return None;
    }

    let year = input[..4].parse::<u16>().ok()?;
    let month = input[5..7].parse::<u8>().ok()?;
    let day = input[8..10].parse::<u8>().ok()?;
    let hour = input[11..13].parse::<u8>().ok()?;
    let minute = input[14..16].parse::<u8>().ok()?;
    let second = input[17..19].parse::<u8>().ok()?;
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let maximum_day = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return None,
    };
    // The Unix conversion cannot verify leap seconds, so strict evidence rejects them.
    if year < 1970 || day == 0 || day > maximum_day || hour > 23 || minute > 59 || second > 59 {
        return None;
    }

    let suffix = &input[19..];
    let zone = if let Some(fraction) = suffix.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        &fraction[digits..]
    } else {
        suffix
    };
    if zone != "Z" && zone != "z" {
        let offset = zone.as_bytes();
        if offset.len() != 6
            || !matches!(offset[0], b'+' | b'-')
            || offset[3] != b':'
            || !offset[1..3].iter().all(u8::is_ascii_digit)
            || !offset[4..6].iter().all(u8::is_ascii_digit)
            || zone[1..3].parse::<u8>().ok()? > 23
            || zone[4..6].parse::<u8>().ok()? > 59
        {
            return None;
        }
    }
    super::parse_rfc3339_to_epoch_secs(input)
}

#[cfg(test)]
mod tests {
    use super::super::{parse_usage_report, WeeklyUsageEvidence};
    use serde_json::{json, Value};

    #[test]
    fn strict_reports_accept_complete_supported_resets() {
        for (reset, epoch) in [
            ("1970-01-01T00:00:00Z", 0),
            ("2021-01-01T00:00:00Z", 1_609_459_200),
            ("2021-01-01T00:00:00z", 1_609_459_200),
            ("2021-01-01T00:00:00.0Z", 1_609_459_200),
            ("2021-01-01T00:00:00.123456789z", 1_609_459_200),
            ("2021-01-01T00:00:00.123456+00:00", 1_609_459_200),
            ("2021-01-01T23:59:59Z", 1_609_545_599),
            ("2021-01-01T23:59:00+23:59", 1_609_459_200),
            ("2021-01-01T09:00:00+09:00", 1_609_459_200),
            ("2020-12-31T19:00:00-05:00", 1_609_459_200),
            ("2020-12-31T00:01:00-23:59", 1_609_459_200),
            ("2000-02-29T00:00:00Z", 951_782_400),
            ("2024-02-29T00:00:00Z", 1_709_164_800),
        ] {
            let report = parse_usage_report(&json!({
                "seven_day": {"utilization": 100, "resets_at": reset}
            }));
            let WeeklyUsageEvidence::Reported(window) = report.weekly else {
                panic!("valid reset must supply strict evidence: {reset}");
            };
            assert_eq!(window.utilization, 1.0);
            assert_eq!(window.resets_at, Some(epoch), "{reset}");
            assert_eq!(report.usage.seven_day, Some(window));
        }
    }

    #[test]
    fn strict_reports_reject_incomplete_or_invalid_resets() {
        for reset in [
            "",
            "2021-01-01T00:00:00.garbageZ",
            "2021-01-01T00:00:00.Z",
            "2021-01-01T00:00:00.1.2Z",
            "2021-01-01T00:00:00.1e2Z",
            "2021-01-01T00:00:00.+1Z",
            "2021-01-01T00:00:00.éZ",
            "2021-01-01T00:00:00.1 Z",
            "2021-01-01T00:00:00Z ",
            "2021-01-01T00:00:00Z\n",
            " 2021-01-01T00:00:00Z",
            "2021-01-01t00:00:00Z",
            "2021-01-01 00:00:00Z",
            "2021-01-01T00:00:00TZ",
            "2021-01-01T00:00:00ZZ",
            "2021-01-01T00:00:00Zextra",
            "+2021-01-01T00:00:00Z",
            "02021-01-01T00:00:00Z",
            "970-01-01T00:00:00Z",
            "10000-01-01T00:00:00Z",
            "1969-12-31T23:59:59Z",
            "2021-1-01T00:00:00Z",
            "2021-01-1T00:00:00Z",
            "2021-+1-01T00:00:00Z",
            "2021-01-+1T00:00:00Z",
            "2021-01-01T0:00:00Z",
            "2021-01-01T00:0:00Z",
            "2021-01-01T00:00:0Z",
            "2021-01-01T+0:00:00Z",
            "2021-01-01T00:+0:00Z",
            "2021-01-01T00:00:+0Z",
            "2021-00-01T00:00:00Z",
            "2021-13-01T00:00:00Z",
            "2021-01-00T00:00:00Z",
            "2021-01-32T00:00:00Z",
            "2021-04-31T00:00:00Z",
            "2021-06-31T00:00:00Z",
            "2021-09-31T00:00:00Z",
            "2021-11-31T00:00:00Z",
            "2025-02-29T00:00:00Z",
            "2100-02-29T00:00:00Z",
            "2000-02-30T00:00:00Z",
            "2021-01-01T24:00:00Z",
            "2021-01-01T00:60:00Z",
            "2021-01-01T00:00:60Z",
            "2016-12-31T23:59:60Z",
            "2017-01-01T08:59:60+09:00",
            "2021-01-01T00:00:61Z",
            "2021-01-01T00:00:00",
            "2021-01-01T00:00:00+00",
            "2021-01-01T00:00:00+0000",
            "2021-01-01T00:00:00+0:00",
            "2021-01-01T00:00:00+00:0",
            "2021-01-01T00:00:00+24:00",
            "2021-01-01T00:00:00+00:60",
            "2021-01-01T00:00:00-00:-1",
            "2021-01-01T00:00:00.1",
            "2021-01-01T00:00:00.1+00:00extra",
        ] {
            let report = parse_usage_report(&json!({
                "seven_day": {"utilization": 100, "resets_at": reset}
            }));
            assert_eq!(report.weekly, WeeklyUsageEvidence::Invalid, "{reset}");
            assert_eq!(report.usage.seven_day.unwrap().utilization, 1.0);
        }
        for reset in [Value::Null, json!(123), json!(false), json!([]), json!({})] {
            let report = parse_usage_report(&json!({
                "seven_day": {"utilization": 100, "resets_at": reset}
            }));
            assert_eq!(report.weekly, WeeklyUsageEvidence::Invalid);
            assert_eq!(report.usage.seven_day.unwrap().resets_at, None);
        }
        let missing = parse_usage_report(&json!({"seven_day": {"utilization": 100}}));
        assert_eq!(missing.weekly, WeeklyUsageEvidence::Invalid);
        assert_eq!(missing.usage.seven_day.unwrap().resets_at, None);
        assert_eq!(
            parse_usage_report(&json!({})).weekly,
            WeeklyUsageEvidence::Unreported
        );
    }

    #[test]
    fn strict_validation_preserves_permissive_legacy_reset_results() {
        for (reset, epoch) in [
            ("2021-01-01T00:00:00.garbageZ", 1_609_459_200),
            ("2021-01-01T00:00:00.Z", 1_609_459_200),
            ("2021-01-01T00:00:00.1.2Z", 1_609_459_200),
            ("2021-01-01T00:00:00+00", 1_609_459_200),
            ("2021-1-1T0:0:0Z", 1_609_459_200),
            ("2021-02-29T00:00:00Z", 1_614_556_800),
            ("2016-12-31T23:59:60Z", 1_483_228_800),
        ] {
            let report = parse_usage_report(&json!({
                "seven_day": {"utilization": 100, "resets_at": reset}
            }));
            assert_eq!(report.weekly, WeeklyUsageEvidence::Invalid);
            let legacy = report.usage.seven_day.unwrap();
            assert_eq!(legacy.utilization, 1.0);
            assert_eq!(legacy.resets_at, Some(epoch), "{reset}");
        }
    }
}
