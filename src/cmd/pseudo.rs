static USAGE: &str = r#"
Pseudonymise the value of a given column by replacing it with an
incremental identifier. See https://en.wikipedia.org/wiki/Pseudonymization

Once a value is pseudonymised, it will always be replaced with the same
identifier. This means that the same value will always be replaced with
the same identifier, even if it appears in different rows.

The incremental identifier is generated by using the given format string
and the starting number and increment.

EXAMPLE:

Pseudonymise the value of the "Name" column by replacing it with an
incremental identifier starting at 1000 and incrementing by 5:

    $ qsv pseudo Name --start 1000 --increment 5 --fmtstr "ID-{}" data.csv

If run on the following CSV data:

    Name,Color
    Mary,yellow
    John,blue
    Mary,purple
    Sue,orange
    John,magenta
    Mary,cyan

 will replace the value of the "Name" column with the following values:

    Name,Color
    ID-1000,yellow
    ID-1005,blue
    ID-1000,purple
    ID-1010,orange
    ID-1005,magenta
    ID-1000,cyan

For more examples, see https://github.com/jqnatividad/qsv/blob/master/tests/test_pseudo.rs.

Usage:
    qsv pseudo [options] <column> [<input>]
    qsv pseudo --help

pseudo arguments:
    <column>                The column to pseudonymise. You can use the `--select`
                            option to select the column by name or index.
                            See `select` command for more details.
    <input>                 The CSV file to read from. If not specified, then
                            the input will be read from stdin.

Common options:
    -h, --help              Display this message
    --start <number>        The starting number for the incremental identifier.
                            [default: 0]
    --increment <number>    The increment for the incremental identifier.
                            [default: 1]
    --formatstr <template>  The format string for the incremental identifier.
                            The format string must contain a single "{}" which
                            will be replaced with the incremental identifier.
                            [default: {}]
    -o, --output <file>     Write output to <file> instead of stdout.
    -n, --no-headers        When set, the first row will not be interpreted
                            as headers.
    -d, --delimiter <arg>   The field delimiter for reading CSV data.
                            Must be a single character. (default: ,)
"#;

use ahash::AHashMap;
use dynfmt::Format;
use serde::Deserialize;

use crate::{
    config::{Config, Delimiter},
    select::SelectColumns,
    util,
    util::replace_column_value,
    CliResult,
};

#[derive(Deserialize)]
struct Args {
    arg_column:      SelectColumns,
    arg_input:       Option<String>,
    flag_start:      u64,
    flag_increment:  u64,
    flag_formatstr:  String,
    flag_output:     Option<String>,
    flag_no_headers: bool,
    flag_delimiter:  Option<Delimiter>,
}

type Values = AHashMap<String, String>;
type ValuesNum = AHashMap<String, u64>;

pub fn run(argv: &[&str]) -> CliResult<()> {
    let args: Args = util::get_args(USAGE, argv)?;
    let rconfig = Config::new(args.arg_input.as_ref())
        .delimiter(args.flag_delimiter)
        .no_headers(args.flag_no_headers)
        .select(args.arg_column);

    let mut rdr = rconfig.reader()?;
    let mut wtr = Config::new(args.flag_output.as_ref()).writer()?;

    let headers = rdr.byte_headers()?.clone();
    let column_index = match rconfig.selection(&headers) {
        Ok(sel) => {
            let sel_len = sel.len();
            if sel_len > 1 {
                return fail_incorrectusage_clierror!(
                    "{sel_len} columns selected. Only one column can be selected for \
                     pseudonymisation."
                );
            }
            // safety: we checked that sel.len() == 1
            *sel.iter().next().unwrap()
        },
        Err(e) => return fail_clierror!("{e}"),
    };

    if !rconfig.no_headers {
        wtr.write_record(&headers)?;
    }

    let mut record = csv::StringRecord::new();
    let mut counter: u64 = args.flag_start;
    let increment = args.flag_increment;
    let mut curr_counter: u64 = 0;
    let mut overflowed = false;

    if args.flag_formatstr == "{}" {
        // we don't need to use dynfmt::SimpleCurlyFormat if the format string is "{}"
        let mut values_num = ValuesNum::with_capacity(1000);

        while rdr.read_record(&mut record)? {
            let value = record[column_index].to_owned();
            let new_value = values_num.entry(value.clone()).or_insert_with(|| {
                curr_counter = counter;
                (counter, overflowed) = counter.overflowing_add(increment);
                curr_counter
            });
            if overflowed {
                return fail_incorrectusage_clierror!(
                    "Overflowed. The counter is larger than u64::MAX {}. The last valid counter \
                     is {curr_counter}.",
                    u64::MAX
                );
            }
            record = replace_column_value(&record, column_index, &new_value.to_string());

            wtr.write_record(&record)?;
        }
    } else {
        // we need to use dynfmt::SimpleCurlyFormat if the format string is not "{}"

        // first, validate the format string
        if !args.flag_formatstr.contains("{}")
            || dynfmt::SimpleCurlyFormat
                .format(&args.flag_formatstr, [0])
                .is_err()
        {
            return fail_incorrectusage_clierror!(
                "Invalid format string: \"{}\". The format string must contain a single \"{{}}\" \
                 which will be replaced with the incremental identifier.",
                args.flag_formatstr
            );
        }

        let mut values = Values::with_capacity(1000);
        while rdr.read_record(&mut record)? {
            let value = record[column_index].to_owned();

            // safety: we checked that the format string contains "{}"
            let new_value = values.entry(value.clone()).or_insert_with(|| {
                curr_counter = counter;
                (counter, overflowed) = counter.overflowing_add(increment);
                dynfmt::SimpleCurlyFormat
                    .format(&args.flag_formatstr, [curr_counter])
                    .unwrap()
                    .to_string()
            });
            if overflowed {
                return fail_incorrectusage_clierror!(
                    "Overflowed. The counter is larger than u64::MAX({}). The last valid counter \
                     is {curr_counter}.",
                    u64::MAX
                );
            }

            record = replace_column_value(&record, column_index, new_value);
            wtr.write_record(&record)?;
        }
    }

    Ok(wtr.flush()?)
}
