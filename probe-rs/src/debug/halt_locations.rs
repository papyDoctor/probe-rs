use super::{DebugError, DebugInfo, SourceLocation};

/// Program row data that the debugger can use for breakpoints and stepping.
/// To understand how this struct is used, use the following framework:
/// - Everything is calculated from a given machine instruction address, usually the current program counter.
/// - To calculate where the user might step to (step-over, step-into, step-out), we start from the given instruction address/program counter, and work our way through the subsequent sequence of instructions. A sequence of instructions represents a series of contiguous target machine instructions, and does not necessarily represent the whole of a function.
/// - The next address in the target processor's instruction sequence may qualify as (one, or more) of the following:
///   - The start of a new source statement (a source file may have multiple statements on a single line)
///   - Another instruction that is part of the source statement started previously
///   - The first instruction after the end of the sequence epilogue.
///   - The end of the current sequence of instructions.
///   - DWARF defines other flags that are not relevant/used here.
/// - Depending on the combinations of the above, we only use instructions that qualify as:
///   - The beginning of a statement that is neither inside the prologue, nor inside the epilogue.
/// - Based on this, we will attempt to fill the [`HaltLocations`] struct with as many of the four fields as possible, given the available information in the instruction sequence.
/// All data is calculated using the `gimli::read::CompletedLineProgram` as well as, function call data from the debug info frame section.
/// NOTE: Sometimes the target program_counter is at a location where the debug_info program row data does not contain valid statements for halt points, and we will return a DebugError::NoValidHaltLocation . In this case, we recommend the consumer of this API step the core to the next instruction and try again, with a resasonable retry limit.
#[derive(Debug)]
pub struct HaltLocations {
    /// For when we are trying to determine a 'source breakpoint', this is the first valid statement past the program counter, where we can set a breakpoint.
    ///  - If the current program_counter is in the prologue of a sequence, then this is the address of the first statement past the end of the prologue.
    ///  - If the current program counter is a call to an inlined function, then use the next statement as a representative of the first statement in that function.
    ///  - If the current program counter is a call to a non-inlined function, then use the first post-prologue statement in that function.
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
    /// -- For example, if the consumer wants to step over the current statement, then the `next_statement_address` is the best choice.
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

        let (program_unit, complete_line_program, active_sequence) =
            get_program_info_at_pc(debug_info, program_counter)?;
        let mut sequence_rows = complete_line_program.resume_from(&active_sequence);

        // By definition, only the addresses INSIDE a sequence will increase monotonically, so we have to be careful when using addresses as comparators.
        let mut prologue_completed = false;

        while let Ok(Some((program_header, row))) = sequence_rows.next_row() {
            log::trace!("Evaluating program row data @{:#010X}  stmt={:5}  ep={:5}  es={:5}  line={:04}  col={:05}  f={:02}",
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

            // Don't do anything until we are at least at the prologue_end() of a function.
            if row.prologue_end() {
                prologue_completed = true;
            }
            if !prologue_completed {
                continue;
            }

            // row.end_sequence() is a row whose address is that of the byte after the last target machine instruction of the sequence.
            // - At this point, the program_counter register is no longer inside the code of the sequence.
            // - IMPORTANT: Because of the above, we will NOT allow a breakpoint, or a step target to be on a statement that is a row.end_sequence()

            // PART 1: Set the first_breakpoint_address
            if program_row_data.first_halt_address.is_none() && row.address() >= program_counter {
                if row.end_sequence() {
                    // If the first non-prologue row is a end of sequence, then we cannot determine valid halt addresses at this program counter.
                    return Err(DebugError::NoValidHaltLocation{
                            message: "This function does not have any valid halt locations. Please consider using instruction level stepping.".to_string(),
                            pc_at_error: program_counter,
                        });
                } else if row.is_stmt() {
                    // We have a good first halt address.
                    program_row_data.first_halt_address = Some(row.address());
                    if let Some(file_entry) = row.file(program_header) {
                        if let Some((file, directory)) = debug_info.find_file_and_directory(
                            &program_unit.unit,
                            program_header,
                            file_entry,
                        ) {
                            program_row_data.first_halt_source_location = Some(SourceLocation {
                                line: row.line().map(std::num::NonZeroU64::get),
                                column: Some(row.column().into()),
                                file,
                                directory,
                                low_pc: Some(active_sequence.start as u32),
                                high_pc: Some(active_sequence.end as u32),
                            });
                        }
                    }
                    // We can move to the next row until we find the next_statement_address.
                    continue;
                } else {
                    continue;
                }
            }

            // PART 2: Set the next_statement_address
            if program_row_data.first_halt_address.is_some()
                && program_row_data.next_statement_address.is_none()
                && row.address() > program_counter
            {
                if row.end_sequence() {
                    // If the next row is a end of sequence, then we cannot determine valid halt addresses at this program counter. We will assign a value in PART 5.
                    break;
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

        // PART 3: In the unlikely scenario that we encounter a sequence of statements that complete before we encounter `row.prologue_end()`, then we will arrive at this point with no halt location information.
        if program_row_data.first_halt_address.is_none() {
            Err(DebugError::NoValidHaltLocation{
                message: "This function does not have any valid halt locations. Please consider using instruction level stepping.".to_string(),
                pc_at_error: program_counter,
            })
        } else {
            // PART 4: This is a safe time to determine the step_out_address.
            if return_address.is_none() {
                // This happens because we don't need to calculate the 'step_out' address ...
                // - When setting breakpoints.
                // - When processing inlined functions as a recursive call to HaltLocations.new() (See below).
            } else if let Ok(function_dies) =
                program_unit.get_function_dies(program_counter, None, true)
            {
                // We want the first qualifying (PC is in range) function from the back of this list, to access the 'inntermost' functions first.
                for function in function_dies.iter().rev() {
                    println!(
                        "Evaluating function {:?}, low_pc={:?}, high_pc={:?}",
                        function.function_name(),
                        function.low_pc,
                        function.high_pc
                    );
                    if function.low_pc <= program_counter as u64
                        && function.high_pc > program_counter as u64
                    {
                        if function.is_inline() {
                            // Step_out_address for inlined functions, is the first available breakpoint address after the last statement in this function.

                            if let Some(first_halt_address) =
                                HaltLocations::new(debug_info, function.high_pc, return_address)?
                                    .first_halt_address
                            {
                                program_row_data.step_out_address = Some(first_halt_address);
                            } else {
                                return Err(DebugError::NoValidHaltLocation{
                                    message: "Could not determinea valid step-out location. Please consider using instruction level stepping.".to_string(),
                                    pc_at_error: program_counter,
                                });
                            }
                        } else if function.get_attribute(gimli::DW_AT_noreturn).is_some() {
                            // This is the only condition that will result in a `None` step_out_address. All other situations will result in a `Some` step_out_address or an Err().
                            log::debug!(
                                "Found DW_AT_noreturn option for function {:?}. Cannot step out.",
                                function.function_name()
                            );
                        } else if program_row_data.step_out_address.is_none() {
                            // Step_out_address for non-inlined functions is the first available breakpoint address after the return address.
                            if let Some(return_address) = return_address {
                                program_row_data.step_out_address =
                                    HaltLocations::new(debug_info, return_address, None)?
                                        .first_halt_address;
                            } else {
                                return Err(DebugError::NoValidHaltLocation{
                                    message: "Could not determinea valid step-out location. Please consider using instruction level stepping.".to_string(),
                                    pc_at_error: program_counter,
                                });
                            }
                        }
                        // We only process the first function that meets this if condition.
                        break;
                    }
                }

                // PART 5: Set the next_halt_address if it is not already set.
                if program_row_data.next_statement_address.is_none() {
                    if let Some(step_out_address) = program_row_data.step_out_address {
                        log::warn!(
                        "No next statement address found for {:?}. Using the step_out address {:?}.",
                        program_counter, &step_out_address
                    );
                        program_row_data.next_statement_address = Some(step_out_address);
                    }
                }
            }

            Ok(program_row_data)
        }
    }
}

// Overriding clippy, as this is a private helper function.
#[allow(clippy::type_complexity)]
/// Resolve the relevant program row data for the given program counter.
fn get_program_info_at_pc(
    debug_info: &DebugInfo,
    program_counter: u64,
) -> Result<
    (
        super::unit_info::UnitInfo,
        gimli::CompleteLineProgram<
            gimli::EndianReader<gimli::LittleEndian, std::rc::Rc<[u8]>>,
            usize,
        >,
        gimli::LineSequence<gimli::EndianReader<gimli::LittleEndian, std::rc::Rc<[u8]>>>,
    ),
    DebugError,
> {
    let program_unit = get_compile_unit_info(debug_info, program_counter)?;
    let (offset, address_size) = if let Some(line_program) = program_unit.unit.line_program.clone()
    {
        (
            line_program.header().offset(),
            line_program.header().address_size(),
        )
    } else {
        return Err(DebugError::NoValidHaltLocation{
                    message: "The specified source location does not have any line_program information available. Please consider using instruction level stepping.".to_string(),
                    pc_at_error: program_counter,
                });
    };

    // Get the sequences of rows from the CompleteLineProgram at the given program_counter.
    let incomplete_line_program =
        debug_info
            .debug_line_section
            .program(offset, address_size, None, None)?;
    let (complete_line_program, line_sequences) = incomplete_line_program.sequences()?;

    // Get the sequence of rows that belongs to the program_counter.
    if let Some(active_sequence) = line_sequences.iter().find(|line_sequence| {
        line_sequence.start <= program_counter && program_counter < line_sequence.end
    }) {
        Ok((program_unit, complete_line_program, active_sequence.clone()))
    } else {
        Err(DebugError::NoValidHaltLocation{
                    message: "The specified source location does not have any line information available. Please consider using instruction level stepping.".to_string(),
                    pc_at_error: program_counter,
                })
    }
}

/// First we have to find the compile unit at the current address.
fn get_compile_unit_info(
    debug_info: &DebugInfo,
    program_counter: u64,
) -> Result<super::unit_info::UnitInfo, DebugError> {
    let mut units = debug_info.get_units();
    while let Some(header) = debug_info.get_next_unit_info(&mut units) {
        match debug_info.dwarf.unit_ranges(&header.unit) {
            Ok(mut ranges) => {
                while let Ok(Some(range)) = ranges.next() {
                    if (range.begin <= program_counter) && (range.end > program_counter) {
                        return Ok(header);
                    }
                }
            }
            Err(_) => continue,
        };
    }
    Err(DebugError::NoValidHaltLocation{
        message: "The specified source location does not have any debug information available. Please consider using instruction level stepping.".to_string(),
        pc_at_error: program_counter,
    })
}
