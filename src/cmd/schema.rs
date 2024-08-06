static USAGE: &str = r#"
Generate JSON Schema from CSV data.

This command derives a JSON Schema Validation (Draft 7) file from CSV data, 
including validation rules based on data type and input data domain/range.
https://json-schema.org/draft/2020-12/json-schema-validation.html

Running `validate` command on original input CSV with generated schema 
should not flag any invalid records.

The intended workflow is to use `schema` command to generate a schema file from
representative CSV data, fine-tune the schema file as needed, and then use `validate`
command to validate other CSV data with the same structure using the generated schema.

Generated schema file has `.schema.json` suffix appended. For example, 
for input `mydata.csv`, schema file would be `mydata.csv.schema.json`.

If piped from stdin, then schema file would be `stdin.csv.schema.json` and
a `stdin.csv` file will created with stdin's contents as well.

Note that `stdin.csv` will be overwritten if it already exists.

Schema generation can be a compute-intensive process, especially for large CSV files.
To speed up generation, the `schema` command will reuse a `stats.csv.bin.sz` file if it
exists and is current (i.e. stats generated with --cardinality and --infer-dates options).
Otherwise, it will run the `stats` command to generate the `stats.csv.bin.sz` file first,
and then use that to generate the schema file.

For examples, see https://github.com/jqnatividad/qsv/blob/master/tests/test_schema.rs.

Usage:
    qsv schema [options] [<input>]
    qsv schema --help

Schema options:
    --enum-threshold <num>     Cardinality threshold for adding enum constraints.
                               Enum constraints are compiled for String & Integer types.
                               [default: 50]
    -i, --ignore-case          Ignore case when compiling unique values for enum constraints.
                               Do note however that the `validate` command is case-sensitive
                               when validating against enum constraints.
    --strict-dates             Enforce Internet Datetime format (RFC-3339) for
                               detected date/datetime columns. Otherwise, even if
                               columns are inferred as date/datetime, they are set
                               to type "string" in the schema instead of
                               "date" or "date-time".
    --pattern-columns <args>   Select columns to derive regex pattern constraints.
                               That is, this will create a regular expression
                               that matches all values for each specified column.
                               Columns are selected using `select` syntax 
                               (see `qsv select --help` for details).
    --dates-whitelist <list>   The case-insensitive patterns to look for when 
                               shortlisting fields for date inference.
                               i.e. if the field's name has any of these patterns,
                               it is shortlisted for date inferencing.
                               Set to "all" to inspect ALL fields for
                               date/datetime types.
                               [default: date,time,due,open,close,created]
    --prefer-dmy               Prefer to parse dates in dmy format.
                               Otherwise, use mdy format.
    --force                    Force recomputing cardinality and unique values
                               even if stats cache file exists and is current.
    --stdout                   Send generated JSON schema file to stdout instead.
    -j, --jobs <arg>           The number of jobs to run in parallel.
                               When not set, the number of jobs is set to the
                               number of CPUs detected.

Common options:
    -h, --help                 Display this message
    -n, --no-headers           When set, the first row will not be interpreted
                               as headers. Namely, it will be processed with the rest
                               of the rows. Otherwise, the first row will always
                               appear as the header row in the output.
    -d, --delimiter <arg>      The field delimiter for reading CSV data.
                               Must be a single character. [default: ,]
    --memcheck                 Check if there is enough memory to load the entire
                               CSV into memory using CONSERVATIVE heuristics.
"#;

use std::{
    fs::File,
    io::{BufReader, Write},
    path::Path,
};

use ahash::{AHashMap, AHashSet};
use csv::ByteRecord;
use grex::RegExpBuilder;
use itertools::Itertools;
use log::{debug, error, info, warn};
use rayon::slice::ParallelSliceMut;
use serde::Deserialize;
use serde_json::{json, value::Number, Map, Value};
use stats::Frequencies;

use crate::{
    cmd::stats::Stats,
    config::{Config, Delimiter, DEFAULT_RDR_BUFFER_CAPACITY},
    select::SelectColumns,
    util, CliResult,
};

#[derive(Deserialize, Clone)]
pub struct Args {
    pub flag_enum_threshold:  usize,
    pub flag_ignore_case:     bool,
    pub flag_strict_dates:    bool,
    pub flag_pattern_columns: SelectColumns,
    pub flag_dates_whitelist: String,
    pub flag_prefer_dmy:      bool,
    pub flag_force:           bool,
    pub flag_stdout:          bool,
    pub flag_jobs:            Option<usize>,
    pub flag_no_headers:      bool,
    pub flag_delimiter:       Option<Delimiter>,
    pub arg_input:            Option<String>,
    pub flag_memcheck:        bool,
}

const STDIN_CSV: &str = "stdin.csv";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StatsMode {
    Schema,
    Frequency,
    FrequencyForceStats,
    None,
}

pub fn run(argv: &[&str]) -> CliResult<()> {
    let mut args: Args = util::get_args(USAGE, argv)?;

    // if using stdin, we create a stdin.csv file as stdin is not seekable and we need to
    // open the file multiple times to compile stats/unique values, etc.
    // We use a fixed "stdin.csv" filename instead of a temporary file with random characters
    // so the name of the generated schema.json file is readable and predictable
    // (stdin.csv.schema.json)
    let (input_path, input_filename) = if args.arg_input.is_none() {
        let mut stdin_file = File::create(STDIN_CSV)?;
        let stdin = std::io::stdin();
        let mut stdin_handle = stdin.lock();
        std::io::copy(&mut stdin_handle, &mut stdin_file)?;
        drop(stdin_handle);
        args.arg_input = Some(STDIN_CSV.to_string());
        (STDIN_CSV.to_string(), STDIN_CSV.to_string())
    } else {
        let filename = Path::new(args.arg_input.as_ref().unwrap())
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        (args.arg_input.clone().unwrap(), filename)
    };

    // we're loading the entire file into memory, we need to check avail mem
    util::mem_file_check(
        &std::path::PathBuf::from(&input_path),
        false,
        args.flag_memcheck,
    )?;

    // we can do this directly here, since args is mutable and
    // Config has not been created yet at this point
    args.flag_prefer_dmy = args.flag_prefer_dmy || util::get_envvar_flag("QSV_PREFER_DMY");
    if args.flag_prefer_dmy {
        winfo!("Prefer DMY set.");
    }

    // build schema for each field by their inferred type, min/max value/length, and unique values
    let mut properties_map: Map<String, Value> =
        match infer_schema_from_stats(&args, &input_filename) {
            Ok(map) => map,
            Err(e) => {
                return fail_clierror!(
                    "Failed to infer schema via stats and frequency from {input_filename}: {e}"
                );
            },
        };

    // generate regex pattern for selected String columns
    let pattern_map = generate_string_patterns(&args, &properties_map)?;

    // enrich properties map with pattern constraint for String fields
    for (field_name, field_def) in &mut properties_map {
        // dbg!(&field_name, &field_def);
        if pattern_map.contains_key(field_name) && should_emit_pattern_constraint(field_def) {
            let field_def_map = field_def.as_object_mut().unwrap();
            let pattern = Value::String(pattern_map[field_name].clone());
            field_def_map.insert("pattern".to_string(), pattern.clone());
            winfo!("Added regex pattern constraint for field: {field_name} -> {pattern}");
        }
    }

    // generate list of required fields
    let required_fields = get_required_fields(&properties_map);

    // create final JSON object for output
    let schema = json!({
        "$schema": "https://json-schema.org/draft-07/schema",
        "title": format!("JSON Schema for {input_filename}"),
        "description": "Inferred JSON Schema from QSV schema command",
        "type": "object",
        "properties": Value::Object(properties_map),
        "required": Value::Array(required_fields)
    });

    let schema_pretty = match serde_json::to_string_pretty(&schema) {
        Ok(s) => s,
        Err(e) => return fail_clierror!("Cannot prettify schema json: {e}"),
    };

    if args.flag_stdout {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();

        handle.write_all(schema_pretty.as_bytes())?;
        handle.flush()?;

        info!("Schema written to stdout");
    } else {
        let schema_output_filename = input_path + ".schema.json";
        let mut schema_output_file = File::create(&schema_output_filename)?;

        schema_output_file.write_all(schema_pretty.as_bytes())?;
        schema_output_file.flush()?;

        woutinfo!("Schema written to {schema_output_filename}");
    }

    Ok(())
}

/// Builds JSON MAP object that corresponds to the "properties" object of JSON Schema (Draft 7) by
/// looking at CSV value stats Supported JSON Schema validation vocabularies:
///  * type
///  * enum
///  * minLength
///  * maxLength
///  * min
///  * max
pub fn infer_schema_from_stats(args: &Args, input_filename: &str) -> CliResult<Map<String, Value>> {
    // invoke cmd::stats
    let (csv_fields, csv_stats, stats_col_index_map) = get_stats_records(args, StatsMode::Schema)?;

    // amortize memory allocation
    let mut low_cardinality_column_indices: Vec<usize> =
        Vec::with_capacity(args.flag_enum_threshold);

    // build column selector arg to invoke cmd::frequency with
    let column_select_arg: String = build_low_cardinality_column_selector_arg(
        &mut low_cardinality_column_indices,
        args.flag_enum_threshold,
        &csv_fields,
        &csv_stats,
        &stats_col_index_map,
    );

    // invoke cmd::frequency to get unique values for each field
    let unique_values_map = get_unique_values(args, &column_select_arg)?;

    // map holds "properties" object of json schema
    let mut properties_map: Map<String, Value> = Map::with_capacity(csv_fields.len());

    // amortize memory allocations
    let mut field_map: Map<String, Value> = Map::with_capacity(10);
    let mut type_list: Vec<Value> = Vec::with_capacity(4);
    let mut enum_list: Vec<Value> = Vec::with_capacity(args.flag_enum_threshold);
    let mut header_byte_slice;
    let mut header_string;
    let mut stats_record;
    let mut col_type;
    let mut col_null_count;

    // generate definition for each CSV column/field and add to properties_map
    for i in 0..csv_fields.len() {
        header_byte_slice = csv_fields.get(i).unwrap();

        // convert csv header to string
        header_string = convert_to_string(header_byte_slice)?;

        // grab stats record for current column
        stats_record = csv_stats.get(i).unwrap().clone().to_record(4, false);

        if log::log_enabled!(log::Level::Debug) {
            debug!("stats[{header_string}]: {stats_record:?}");
        }

        // get Type from stats record
        col_type = stats_record.get(stats_col_index_map["type"]).unwrap();
        // get NullCount
        col_null_count = if let Some(s) = stats_record.get(stats_col_index_map["nullcount"]) {
            s.parse::<usize>().unwrap_or(0_usize)
        } else {
            0_usize
        };

        // debug!(
        //     "{header_string}: type={col_type}, optional={}",
        //     col_null_count > 0
        // );

        // map for holding field definition
        field_map.clear();
        let desc = format!("{header_string} column from {input_filename}");
        field_map.insert("description".to_string(), Value::String(desc));

        // use list to hold types, since optional fields get appended a "null" type
        type_list.clear();
        enum_list.clear();

        match col_type {
            "String" => {
                type_list.push(Value::String("string".to_string()));

                // minLength constraint
                if let Some(min_length_str) = stats_record.get(stats_col_index_map["min_length"]) {
                    let min_length = min_length_str.parse::<u32>().unwrap();
                    field_map.insert(
                        "minLength".to_string(),
                        Value::Number(Number::from(min_length)),
                    );
                };

                // maxLength constraint
                if let Some(max_length_str) = stats_record.get(stats_col_index_map["max_length"]) {
                    let max_length = max_length_str.parse::<u32>().unwrap();
                    field_map.insert(
                        "maxLength".to_string(),
                        Value::Number(Number::from(max_length)),
                    );
                };

                // enum constraint
                if let Some(values) = unique_values_map.get(&header_string) {
                    for value in values {
                        enum_list.push(Value::String(value.to_string()));
                    }
                }
            },
            "Integer" => {
                type_list.push(Value::String("integer".to_string()));

                if let Some(min_str) = stats_record.get(stats_col_index_map["min"]) {
                    let min = atoi_simd::parse::<i64>(min_str.as_bytes()).unwrap();
                    field_map.insert("minimum".to_string(), Value::Number(Number::from(min)));
                };

                if let Some(max_str) = stats_record.get(stats_col_index_map["max"]) {
                    let max = atoi_simd::parse::<i64>(max_str.as_bytes()).unwrap();
                    field_map.insert("maximum".to_string(), Value::Number(Number::from(max)));
                };

                // enum constraint
                if let Some(values) = unique_values_map.get(&header_string) {
                    for value in values {
                        let int_value = atoi_simd::parse::<i64>(value.as_bytes()).unwrap();
                        enum_list.push(Value::Number(Number::from(int_value)));
                    }
                }
            },
            "Float" => {
                type_list.push(Value::String("number".to_string()));

                if let Some(min_str) = stats_record.get(stats_col_index_map["min"]) {
                    let min = min_str.parse::<f64>().unwrap();
                    field_map.insert(
                        "minimum".to_string(),
                        Value::Number(Number::from_f64(min).unwrap()),
                    );
                };

                if let Some(max_str) = stats_record.get(stats_col_index_map["max"]) {
                    let max = max_str.parse::<f64>().unwrap();
                    field_map.insert(
                        "maximum".to_string(),
                        Value::Number(Number::from_f64(max).unwrap()),
                    );
                };
            },
            "NULL" => {
                type_list.push(Value::String("null".to_string()));
            },
            "Date" => {
                type_list.push(Value::String("string".to_string()));

                if args.flag_strict_dates {
                    field_map.insert("format".to_string(), Value::String("date".to_string()));
                }
            },
            "DateTime" => {
                type_list.push(Value::String("string".to_string()));

                if args.flag_strict_dates {
                    field_map.insert("format".to_string(), Value::String("date-time".to_string()));
                }
            },
            _ => {
                wwarn!("Stats gave unexpected field type '{col_type}', default to JSON String.");
                // defaults to JSON String
                type_list.push(Value::String("string".to_string()));
            },
        }

        if col_null_count > 0 && !type_list.contains(&Value::String("null".to_string())) {
            // for fields that are not mandatory,
            // having JSON String "null" in Type lists indicates that value can be missing
            type_list.push(Value::String("null".to_string()));
        }

        if col_null_count > 0 && !enum_list.is_empty() {
            // for fields that are not mandatory and actually have enum list generated,
            // having JSON NULL indicates that missing value is allowed
            enum_list.push(Value::Null);
        }

        if !type_list.is_empty() {
            field_map.insert("type".to_string(), Value::Array(type_list.clone()));
        }

        if !enum_list.is_empty() {
            field_map.insert("enum".to_string(), Value::Array(enum_list.clone()));
            winfo!(
                "Enum list generated for field '{header_string}' ({} value/s)",
                enum_list.len()
            );
        }

        // add current field definition to properties map
        properties_map.insert(header_string, Value::Object(field_map.clone()));
    }

    Ok(properties_map)
}

/// get stats records from stats.bin file, or if its invalid, by running the stats command
/// returns tuple (`csv_fields`, `csv_stats`, `stats_col_index_map`)
pub fn get_stats_records(
    args: &Args,
    mode: StatsMode,
) -> CliResult<(ByteRecord, Vec<Stats>, AHashMap<String, usize>)> {
    let stats_args = crate::cmd::stats::Args {
        arg_input:            args.arg_input.clone(),
        flag_select:          crate::select::SelectColumns::parse("").unwrap(),
        flag_everything:      false,
        flag_typesonly:       false,
        flag_infer_boolean:   false,
        flag_mode:            false,
        flag_cardinality:     true,
        flag_median:          false,
        flag_quartiles:       false,
        flag_mad:             false,
        flag_nulls:           false,
        flag_round:           4,
        flag_infer_dates:     true,
        flag_dates_whitelist: args.flag_dates_whitelist.to_string(),
        flag_prefer_dmy:      args.flag_prefer_dmy,
        flag_force:           args.flag_force,
        flag_jobs:            Some(util::njobs(args.flag_jobs)),
        flag_stats_binout:    true,
        flag_cache_threshold: 1, // force the creation of stats cache files
        flag_output:          None,
        flag_no_headers:      args.flag_no_headers,
        flag_delimiter:       args.flag_delimiter,
        flag_memcheck:        args.flag_memcheck,
    };

    let canonical_input_path = Path::new(&args.arg_input.clone().unwrap()).canonicalize()?;
    let stats_binary_encoded_path = canonical_input_path.with_extension("stats.csv.bin.sz");

    let stats_bin_current = if stats_binary_encoded_path.exists() {
        let stats_bin_metadata = std::fs::metadata(&stats_binary_encoded_path)?;

        let input_metadata = std::fs::metadata(args.arg_input.clone().unwrap())?;

        if stats_bin_metadata.modified()? > input_metadata.modified()? {
            info!("Valid stats.csv.bin.sz file found!");
            true
        } else {
            info!("stats.csv.bin.sz file is older than input file. Regenerating stats.bin file.");
            false
        }
    } else {
        info!("stats.csv.bin.sz file does not exist: {stats_binary_encoded_path:?}");
        false
    };

    if mode == StatsMode::None || (mode == StatsMode::Frequency && !stats_bin_current) {
        // if the stats.bin file is not present, we're just doing frequency old school
        // without cardinality
        return Ok((ByteRecord::new(), Vec::new(), AHashMap::new()));
    }

    let mut stats_bin_loaded = false;

    // if stats.bin file exists and is current, use it
    let mut csv_stats: Vec<Stats> = Vec::new();

    if stats_bin_current && !args.flag_force {
        let bin_file = BufReader::with_capacity(
            DEFAULT_RDR_BUFFER_CAPACITY * 4,
            File::open(stats_binary_encoded_path)?,
        );
        let mut buf_binsz_decoder = snap::read::FrameDecoder::new(bin_file);
        match bincode::deserialize_from(&mut buf_binsz_decoder) {
            Ok(stats) => {
                csv_stats = stats;
                stats_bin_loaded = true;
            },
            Err(e) => {
                wwarn!(
                    "Error reading stats.csv.bin.sz file: {e:?}. Regenerating stats.bin.sz file."
                );
            },
        }
    }

    if !stats_bin_loaded {
        // otherwise, run stats command to generate stats.csv.bin.sz file
        let tempfile = tempfile::Builder::new()
            .suffix(".stats.csv")
            .tempfile()
            .unwrap();
        let tempfile_path = tempfile.path().to_str().unwrap().to_string();

        let statsbin_path = canonical_input_path.with_extension("stats.csv.bin.sz");

        let mut stats_args_str = if mode == StatsMode::Schema {
            // mode is GetStatsMode::Schema
            // we're generating schema, so we need all the stats
            format!(
                "stats {input} --infer-dates --dates-whitelist {dates_whitelist} --round 4 \
                 --cardinality --output {output} --stats-binout --force",
                input = {
                    if let Some(arg_input) = stats_args.arg_input.clone() {
                        arg_input
                    } else {
                        "-".to_string()
                    }
                },
                dates_whitelist = stats_args.flag_dates_whitelist,
                output = tempfile_path,
            )
        } else {
            // mode is GetStatsMode::Frequency or GetStatsMode::FrequencyForceStats
            // we're doing frequency, so we just need cardinality
            format!(
                "stats {input} --cardinality --stats-binout --output {output}",
                input = {
                    if let Some(arg_input) = stats_args.arg_input.clone() {
                        arg_input
                    } else {
                        "-".to_string()
                    }
                },
                output = tempfile_path,
            )
        };
        if args.flag_prefer_dmy {
            stats_args_str = format!("{stats_args_str} --prefer-dmy");
        }
        if args.flag_no_headers {
            stats_args_str = format!("{stats_args_str} --no-headers");
        }
        if let Some(delimiter) = args.flag_delimiter {
            let delim = delimiter.as_byte() as char;
            stats_args_str = format!("{stats_args_str} --delimiter {delim}");
        }
        if args.flag_memcheck {
            stats_args_str = format!("{stats_args_str} --memcheck");
        }
        if let Some(mut jobs) = stats_args.flag_jobs {
            if jobs > 2 {
                jobs -= 1; // leave one core for the main thread
            }
            stats_args_str = format!("{stats_args_str} --jobs {jobs}");
        }

        let stats_args_vec: Vec<&str> = stats_args_str.split_whitespace().collect();

        let qsv_bin = std::env::current_exe().unwrap();
        let mut stats_cmd = std::process::Command::new(qsv_bin);
        stats_cmd.args(stats_args_vec);
        let _stats_output = stats_cmd.output()?;

        let bin_file =
            BufReader::with_capacity(DEFAULT_RDR_BUFFER_CAPACITY * 2, File::open(statsbin_path)?);
        let mut buf_binsz_decoder = snap::read::FrameDecoder::new(bin_file);

        match bincode::deserialize_from(&mut buf_binsz_decoder) {
            Ok(stats) => {
                csv_stats = stats;
            },
            Err(e) => {
                return fail_clierror!(
                    "Error reading stats.csv.bin.sz file: {e:?}. Schema generation aborted."
                );
            },
        }
    };

    // get the headers from the input file
    let mut rdr = csv::Reader::from_path(args.arg_input.clone().unwrap()).unwrap();
    let csv_fields = rdr.byte_headers()?.clone();
    drop(rdr);

    let stats_columns = if stats_bin_loaded {
        // if stats.bin file is loaded, we need to get the headers from the stats.csv file
        let stats_bin_csv_path = canonical_input_path.with_extension("stats.csv");
        let mut stats_csv_reader = csv::Reader::from_path(stats_bin_csv_path)?;
        let stats_csv_headers = stats_csv_reader.headers()?.clone();
        drop(stats_csv_reader);
        stats_csv_headers
    } else {
        // otherwise, we generate the headers from the stats_args struct
        // as we used the stats_args struct to generate the stats.csv file
        stats_args.stat_headers()
    };

    let mut stats_col_index_map = AHashMap::new();

    for (i, col) in stats_columns.iter().enumerate() {
        if col != "field" {
            // need offset by 1 due to extra "field" column in headers that's not in stats records
            stats_col_index_map.insert(col.to_owned(), i - 1);
        }
    }

    Ok((csv_fields, csv_stats, stats_col_index_map))
}

/// get column selector argument string for low cardinality columns
fn build_low_cardinality_column_selector_arg(
    low_cardinality_column_indices: &mut Vec<usize>,
    enum_cardinality_threshold: usize,
    csv_fields: &ByteRecord,
    csv_stats: &[Stats],
    stats_col_index_map: &AHashMap<String, usize>,
) -> String {
    low_cardinality_column_indices.clear();

    // identify low cardinality columns
    for i in 0..csv_fields.len() {
        // grab stats record for current column
        let stats_record = csv_stats.get(i).unwrap().clone().to_record(4, false);

        // get Cardinality
        let col_cardinality = match stats_record.get(stats_col_index_map["cardinality"]) {
            Some(s) => s.parse::<usize>().unwrap_or(0_usize),
            None => 0_usize,
        };
        // debug!("column_{i}: cardinality={col_cardinality}");

        if col_cardinality > 0 && col_cardinality <= enum_cardinality_threshold {
            // column selector uses 1-based index
            low_cardinality_column_indices.push(i + 1);
        };
    }

    debug!("low cardinality columns: {low_cardinality_column_indices:?}");

    let column_select_arg: String = low_cardinality_column_indices
        .iter()
        .map(ToString::to_string)
        .join(",");

    column_select_arg
}

/// get frequency tables from `cmd::frequency`
/// returns map of unique values keyed by header
fn get_unique_values(
    args: &Args,
    column_select_arg: &str,
) -> CliResult<AHashMap<String, Vec<String>>> {
    // prepare arg for invoking cmd::frequency
    let freq_args = crate::cmd::frequency::Args {
        arg_input:           args.arg_input.clone(),
        flag_select:         crate::select::SelectColumns::parse(column_select_arg).unwrap(),
        flag_limit:          args.flag_enum_threshold as isize,
        flag_unq_limit:      args.flag_enum_threshold,
        flag_lmt_threshold:  0,
        flag_pct_dec_places: -5,
        flag_other_sorted:   false,
        flag_other_text:     "Other".to_string(),
        flag_asc:            false,
        flag_no_nulls:       true,
        flag_no_trim:        false,
        flag_ignore_case:    args.flag_ignore_case,
        // internal mode for getting frequency tables
        flag_stats_mode:     "_schema".to_string(),
        flag_jobs:           Some(util::njobs(args.flag_jobs)),
        flag_output:         None,
        flag_no_headers:     args.flag_no_headers,
        flag_delimiter:      args.flag_delimiter,
        flag_memcheck:       args.flag_memcheck,
    };

    let (headers, ftables) = match freq_args.rconfig().indexed()? {
        Some(ref mut idx) => freq_args.parallel_ftables(idx),
        _ => freq_args.sequential_ftables(),
    }?;

    let unique_values_map = construct_map_of_unique_values(&headers, &ftables)?;
    Ok(unique_values_map)
}

/// construct map of unique values keyed by header
fn construct_map_of_unique_values(
    freq_csv_fields: &ByteRecord,
    frequency_tables: &[Frequencies<Vec<u8>>],
) -> CliResult<AHashMap<String, Vec<String>>> {
    let mut unique_values_map: AHashMap<String, Vec<String>> = AHashMap::new();
    let mut unique_values = Vec::with_capacity(freq_csv_fields.len());
    // iterate through fields and gather unique values for each field
    for (i, header_byte_slice) in freq_csv_fields.iter().enumerate() {
        unique_values.clear();

        for (val_byte_vec, _count) in frequency_tables[i].most_frequent().0 {
            let val_string = convert_to_string(val_byte_vec.as_slice())?;
            unique_values.push(val_string);
        }

        let header_string = convert_to_string(header_byte_slice)?;

        // sort the values so enum list so schema can be diff'ed between runs
        unique_values.par_sort_unstable();

        // if log::log_enabled!(log::Level::Debug) {
        //     // we do this as this debug is relatively expensive
        //     debug!(
        //         "enum[{header_string}]: len={}, val={:?}",
        //         unique_values.len(),
        //         unique_values
        //     );
        // }
        unique_values_map.insert(header_string, unique_values.clone());
    }
    // dbg!(&unique_values_map);

    Ok(unique_values_map)
}

/// convert byte slice to UTF8 String
#[inline]
fn convert_to_string(byte_slice: &[u8]) -> CliResult<String> {
    // convert csv header to string
    if let Ok(s) = simdutf8::basic::from_utf8(byte_slice) {
        Ok(s.to_string())
    } else {
        let lossy_string = String::from_utf8_lossy(byte_slice);
        fail_clierror!(
            "Can't convert byte slice to utf8 string. slice={byte_slice:?}: {lossy_string}"
        )
    }
}

/// determine required fields
fn get_required_fields(properties_map: &Map<String, Value>) -> Vec<Value> {
    let mut fields: Vec<Value> = Vec::with_capacity(properties_map.len());

    // for CSV, all columns in original input file are assume required
    for key in properties_map.keys() {
        fields.push(Value::String(key.clone()));
    }

    fields
}

/// generate map of regex patterns from selected String column of CSV
fn generate_string_patterns(
    args: &Args,
    properties_map: &Map<String, Value>,
) -> CliResult<AHashMap<String, String>> {
    // standard boiler-plate for reading CSV

    let rconfig = Config::new(&args.arg_input)
        .delimiter(args.flag_delimiter)
        .no_headers(args.flag_no_headers)
        .select(args.flag_pattern_columns.clone());

    let mut rdr = rconfig.reader()?;

    let headers = rdr.byte_headers()?.clone();
    let sel = rconfig.selection(&headers)?;

    let mut pattern_map: AHashMap<String, String> = AHashMap::new();

    // return empty pattern map when:
    //  * no columns are selected
    //  * all columns are selected (by default, all columns are selected when no columns are
    //    explicitly specified)
    if sel.len() == 0 || sel.len() == headers.len() {
        debug!("no pattern columns selected");
        return Ok(pattern_map);
    }

    // Map each Header to its unique Set of values
    let mut unique_values_map: AHashMap<String, AHashSet<String>> = AHashMap::new();

    #[allow(unused_assignments)]
    let mut record = csv::ByteRecord::new();
    let mut header_byte_slice: &[u8];
    let mut header_string: String;
    let mut value_string: String;

    while rdr.read_byte_record(&mut record)? {
        for (i, value_byte_slice) in sel.select(&record).enumerate() {
            // get header based on column index in Selection array
            header_byte_slice = headers.get(sel[i]).unwrap();

            // convert header and value byte arrays to UTF8 strings
            header_string = convert_to_string(header_byte_slice)?;

            // pattern validation only applies to String type, so skip if not String
            if !should_emit_pattern_constraint(&properties_map[&header_string]) {
                continue;
            }

            value_string = convert_to_string(value_byte_slice)?;

            let set = unique_values_map.entry(header_string).or_default();
            set.insert(value_string);
        }
    }

    // build regex pattern for each header
    pattern_map.reserve(unique_values_map.len());
    let mut values: Vec<&String>;
    let mut regexp: String;

    for (header, value_set) in &unique_values_map {
        // Convert Set to Vector
        values = Vec::from_iter(value_set);

        // build regex based on unique values
        regexp = RegExpBuilder::from(&values)
            .with_conversion_of_repetitions()
            .with_minimum_repetitions(2)
            .build();

        pattern_map.insert(header.clone(), regexp);
    }

    // debug!("pattern map: {pattern_map:?}");

    Ok(pattern_map)
}

// only emit "pattern" constraint for String fields without enum constraint
fn should_emit_pattern_constraint(field_def: &Value) -> bool {
    let type_list = field_def[&"type"].as_array().unwrap();
    let has_enum = field_def.get("enum").is_some();

    type_list.contains(&Value::String("string".to_string())) && !has_enum
}
