use super::{DebugError, DebugInfo, SourceLocation};

/// Program row data that the debugger can use for breakpoints and stepping.
/// To understand how this struct is used, use the following framework:
/// - Everything is calculated from a given machine instruction address, usually the current program counter.
/// - To calculate where the user might step to (step-over, step-into, step-out), we start from the given instruction address/program counter, and work our way through the subsequent sequence of instructions. A sequence of instructions represents a series of contiguous target machine instructions, and does not necessarily represent the whole of a function.
/// - The next address in the target processor's instruction sequence may qualify as (one, or more) of the following:
///   - The start of a new source statement (a source file may have multiple statements on a single line)
///   - Another instruction that is part of the source statement started previously
///   - The first instruction after the end of the epilogue.
///   - The end of the current sequence of instructions.
///   - Other indicators that are not relevant/used here.
/// - Depending on the combinations of the above, we only use instructions that qualify as:
///   - The beginning of a statement that is neither inside the prologue, nor inside the epilogue.
/// - Based on this, we will attempt to fill the [`HaltLocations`] struct with as many of the four fields as possible, given the available information in the instruction sequence.
/// All data is calculated using the `gimli::read::CompletedLineProgram` as well as, function call data from the debug info frame section.
#[derive(Debug)]
pub struct HaltLocations {
    /// For when we are trying to determine a 'source breakpoint', this is the first valid statement past the program counter, where we can set a breakpoint.
    ///  - If the current program_counter is in the prologue of a sequence, then this is the address of the first statement past the end of the prologue.
    ///  - If the current program counter is a call to an inlined function, then the next statement will be the first statement in that function.
    pub first_halt_address: Option<u64>,
    /// The source location associated with the first_halt_address.
    pub first_halt_source_location: Option<SourceLocation>,
    /// For when we want to 'step over' the current statement, then this is the address of the next valid statement where we can halt.
    ///  - If the current program counter's statement lies between a prologue and epilogue, the `next_statement_address` will be the next statement to be processed by the target.
    ///  - If the next statement happens to be inside a prologue, then the `next_statement_address` will be the address of the first statement after the prologue.
    ///  - If the next statement happens to be inside an epilogue, then the `next_statement_address` will be the same as the `step_out_address` (see below).
    pub next_statement_address: Option<u64>,
    /// For when we want to 'step out' of the current function, then this is the first statement after the current function returns.
    /// - If this is a regular function, this will be the `return address`.
    /// - If this is an inline function, this will be the statement from which the function was "called", because inline statements are executed before the statements that "call" them.  
    pub step_out_address: Option<u64>,
}

impl HaltLocations {
    /// This function uses [`gimli::read::CompleteLineProgram`] functionality to calculate valid addresses where we can request a halt.
    /// Validity of halt locations are defined as target instructions that live between the end of the prologue, and the start of the end sequence of a [`gimli::read::LineRow`].
    ///
    /// Please refer to [`HaltLocations`] struct for a description of the various halt locations that are available for the given program_counter.
    /// - The consumer will have to choose which of the halt locations best fit the requirement of the current request.
    /// -- For example,
    pub(crate) fn new(
        debug_info: &DebugInfo,
        program_counter: u64,
        return_address: Option<u64>,
    ) -> Result<HaltLocations, DebugError> {
        let mut program_row_data = HaltLocations {
            first_halt_address: None,
            first_halt_source_location: None,
            next_statement_address: None,
            step_out_address: None,
        };
        // First we have to find the compile unit at the current address.
        let mut units = debug_info.get_units();
        let mut program_unit = None;
        'headers: while let Some(header) = debug_info.get_next_unit_info(&mut units) {
            match debug_info.dwarf.unit_ranges(&header.unit) {
                Ok(mut ranges) => {
                    while let Ok(Some(range)) = ranges.next() {
                        if (range.begin <= program_counter) && (range.end > program_counter) {
                            program_unit = Some(header);
                            break 'headers;
                        }
                    }
                }
                Err(_) => continue 'headers,
            };
        }

        // Use the gimli::read::DebugLine::program() to return the rows from the LineProgram.
        // TODO: In theory we can cache the program rows and re-use them, but so far the performance is acceptable.
        if let Some(program_unit) = program_unit.as_ref() {
            if let Some(line_program) = program_unit.unit.line_program.clone() {
                let offset = line_program.header().offset();
                let address_size = line_program.header().address_size();

                let incomplete_line_program =
                    debug_info
                        .debug_line_section
                        .program(offset, address_size, None, None)?;
                let (complete_line_program, line_sequences) =
                    incomplete_line_program.sequences()?;

                if let Some(active_sequence) = line_sequences.iter().find(|line_sequence| {
                    line_sequence.start <= program_counter && program_counter < line_sequence.end
                }) {
                    let mut rows = complete_line_program.resume_from(active_sequence);

                    // By definition, ONLY the addresses inside a sequence will increase monotonically, so we have to be careful when using addresses as comparators.
                    let mut prologue_end = u64::MAX;
                    while let Ok(Some((program_header, row))) = rows.next_row() {
                        println!("Evaluating program row data @{:#010X}  stmt={:5}  ep={:5}  es={:5}  line={:04}  col={:05}  f={:02}",
                                        row.address(),
                                        row.is_stmt(),
                                        row.prologue_end(),
                                        row.end_sequence(),
                                        match row.line() {
                                            Some(line) => line.get(),
                                            None => 0,
                                        },
                                        match row.column() {
                                            gimli::ColumnType::LeftEdge => 0,
                                            gimli::ColumnType::Column(column) => column.get(),
                                        },
                                        row.file_index());

                        // Don't do anything until we are past the prologue of a function.
                        if row.prologue_end() {
                            prologue_end = row.address();
                        }

                        // row.end_sequence() is a row whose address is that of the byte after the last target machine instruction of the sequence.
                        // - At this point, the program_counter register is no longer inside the code of the sequence.
                        // - IMPORTANT: Because of the above, we will NOT allow a breakpoint, or a step target to be on a statement that is a row.end_sequence()

                        // Set the first_breakpoint_address
                        if program_row_data.first_halt_address.is_none()
                            && row.address() >= prologue_end
                            && row.address() >= program_counter
                        {
                            if row.end_sequence() {
                                // If the first non-prologue row is a end of sequence, then we cannot determine valid halt addresses at this program counter.
                                return Err(DebugError::NoValidHaltLocation{
                                    message: "This function does not have any valid halt locations. Please consider using instruction level stepping.".to_string(),
                                    pc_at_error: program_counter,
                                });
                            } else if row.is_stmt() {
                                program_row_data.first_halt_address = Some(row.address());
                                if let Some(file_entry) = row.file(program_header) {
                                    if let Some((file, directory)) = debug_info
                                        .find_file_and_directory(
                                            &program_unit.unit,
                                            program_header,
                                            file_entry,
                                        )
                                    {
                                        program_row_data.first_halt_source_location =
                                            Some(SourceLocation {
                                                line: row.line().map(std::num::NonZeroU64::get),
                                                column: Some(row.column().into()),
                                                file,
                                                directory,
                                                low_pc: Some(active_sequence.start as u32),
                                                high_pc: Some(active_sequence.end as u32),
                                            });
                                    }
                                }
                                // This is a safe time to determine the step_out_statement.
                                // Recursive calls will sometimes need to use a return_address as a program_counter, in which case we skip this part.
                                if return_address.is_some() {
                                    if let Ok(function_dies) =
                                        program_unit.get_function_dies(program_counter, None, true)
                                    {
                                        for function in function_dies {
                                            if function.low_pc <= program_counter as u64
                                                && function.high_pc > program_counter as u64
                                            {
                                                if function.is_inline() {
                                                    // Step_out_address for inlined functions, is the first available breakpoint address after the last statement in this function.
                                                    program_row_data.step_out_address =
                                                        HaltLocations::new(
                                                            debug_info,
                                                            function.high_pc,
                                                            return_address,
                                                        )?
                                                        .first_halt_address;
                                                } else if function
                                                    .get_attribute(gimli::DW_AT_noreturn)
                                                    .is_some()
                                                {
                                                    // Cannot step out of non returning functions.
                                                } else if program_row_data
                                                    .step_out_address
                                                    .is_none()
                                                {
                                                    // Step_out_address for non-inlined functions is the first available breakpoint address after the return address.
                                                    program_row_data.step_out_address =
                                                        return_address.and_then(|return_address| {
                                                            HaltLocations::new(
                                                                debug_info,
                                                                return_address,
                                                                None,
                                                            )
                                                            .map_or(None, |valid_halt_locations| {
                                                                valid_halt_locations
                                                                    .first_halt_address
                                                            })
                                                        });
                                                }
                                            }
                                        }
                                    };
                                }
                                // We can move to the next row until we find the next_statement_address.
                                continue;
                            } else {
                                continue;
                            }
                        }

                        // Set the next_statement_address
                        if program_row_data.first_halt_address.is_some()
                            && program_row_data.next_statement_address.is_none()
                            && row.address() > program_counter
                        {
                            if row.end_sequence() {
                                // If the next row is a end of sequence, then we cannot determine valid halt addresses at this program counter.
                                return Err(DebugError::NoValidHaltLocation{
                                    message: "This function does not have any additional halt locations. Please consider using instruction level stepping.".to_string(),
                                    pc_at_error: program_counter,
                                });
                            } else if row.is_stmt() {
                                // Use the next available statement.
                                program_row_data.next_statement_address = Some(row.address());
                                // We have what we need for now.
                                break;
                            } else {
                                continue;
                            }
                        }
                    }
                } else {
                    return Err(DebugError::NoValidHaltLocation{
                        message: "The specified source location does not have any line information available. Please consider using instruction level stepping.".to_string(),
                        pc_at_error: program_counter,
                    });
                }
            }
        }
        Ok(program_row_data)
    }
}
