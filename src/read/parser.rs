use super::lexitem::*;
use crate::error::*;
use crate::reference_tables;
use crate::structs::*;
use crate::validate::*;
use crate::StrictnessLevel;

use std::cmp;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::str::FromStr;

/// Parse the given file into a PDB struct.
/// Returns an PDBError when it found a BreakingError. Otherwise it returns the PDB with all errors/warnings found while parsing it.
pub fn open(filename: &str, level: StrictnessLevel) -> Result<(PDB, Vec<PDBError>), Vec<PDBError>> {
    // Open a file a use a buffered reader to minimise memory use while immediately lexing the line followed by adding it to the current PDB
    let file = if let Ok(f) = File::open(filename) {
        f
    } else {
        return Err(vec![PDBError::new(ErrorLevel::BreakingError, "Could not open file", "Could not open the specified file, make sure the path is correct, you have permission, and that it is not open in another program.", Context::show(filename))]);
    };
    let reader = BufReader::new(file);
    parse(reader, Context::show(filename), level)
}

/// Parse the input stream into a PDB struct. To allow for direct streaming from sources, like from RCSB.org.
/// Returns an PDBError when it found a BreakingError. Otherwise it returns the PDB with all errors/warnings found while parsing it.
///
/// ## Arguments
/// * `input` - the input stream
/// * `context` - the context of the full stream, to place error messages correctly, for files this is `Context::show(filename)`.
/// * `level` - the strictness level to operate in. If errors are generated which are breaking in the given level the parsing will fail.
pub fn parse<T>(
    input: std::io::BufReader<T>,
    context: Context,
    level: StrictnessLevel,
) -> Result<(PDB, Vec<PDBError>), Vec<PDBError>>
where
    T: std::io::Read,
{
    let mut errors = Vec::new();
    let mut pdb = PDB::new();
    let mut current_model = Model::new(0);
    let mut sequence: HashMap<char, Vec<(usize, usize, Vec<String>)>> = HashMap::new();
    let mut database_references = Vec::new();
    let mut modifications = Vec::new();

    for (mut linenumber, read_line) in input.lines().enumerate() {
        linenumber += 1; // 1 based indexing in files

        let line = if let Ok(l) = read_line {
            l
        } else {
            return Err(vec![PDBError::new(
                ErrorLevel::BreakingError,
                "Could read line",
                &format!(
                    "Could not read line {} while parsing the input file.",
                    linenumber
                ),
                context,
            )]);
        };
        let line_result = lex_line(line, linenumber);

        // Then immediately add this lines information to the final PDB struct
        if let Ok((result, line_errors)) = line_result {
            errors.extend(line_errors);
            match result {
                LexItem::Remark(num, text) => pdb.add_remark(num, text.to_string()),
                LexItem::Atom(
                    hetero,
                    serial_number,
                    name,
                    _,
                    residue_name,
                    chain_id,
                    residue_serial_number,
                    _,
                    x,
                    y,
                    z,
                    occ,
                    b,
                    _,
                    element,
                    charge,
                ) => {
                    let atom = Atom::new(serial_number, name, x, y, z, occ, b, element, charge)
                        .expect("Invalid characters in atom creation");

                    if hetero {
                        current_model.add_hetero_atom(
                            atom,
                            chain_id,
                            residue_serial_number,
                            residue_name,
                        );
                    } else {
                        current_model.add_atom(atom, chain_id, residue_serial_number, residue_name);
                    }
                }
                LexItem::Anisou(s, n, _, _r, _c, _rs, _, factors, _, _e, _ch) => {
                    let mut found = false;
                    for atom in current_model.all_atoms_mut().rev() {
                        if atom.serial_number() == s {
                            atom.set_anisotropic_temperature_factors(factors);
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        println!(
                            "Could not find atom for temperature factors, coupled to atom {} {}",
                            s,
                            n.iter().collect::<String>()
                        )
                    }
                }
                LexItem::Model(number) => {
                    if current_model.atom_count() > 0 {
                        pdb.add_model(current_model)
                    }

                    current_model = Model::new(number);
                }
                LexItem::Scale(n, row) => {
                    if !pdb.has_scale() {
                        pdb.set_scale(Scale::new());
                    }
                    pdb.scale_mut().set_row(n, row);
                }
                LexItem::OrigX(n, row) => {
                    if !pdb.has_origx() {
                        pdb.set_origx(OrigX::new());
                    }
                    pdb.origx_mut().set_row(n, row);
                }
                LexItem::MtriX(n, ser, row, given) => {
                    let mut found = false;
                    for mtrix in pdb.mtrix_mut() {
                        if mtrix.serial_number == ser {
                            mtrix.set_row(n, row);
                            mtrix.contained = given;
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        let mut mtrix = MtriX::new();
                        mtrix.serial_number = ser;
                        mtrix.set_row(n, row);
                        mtrix.contained = given;
                        pdb.add_mtrix(mtrix);
                    }
                }
                LexItem::Crystal(a, b, c, alpha, beta, gamma, spacegroup, _z) => {
                    pdb.set_unit_cell(UnitCell::new(a, b, c, alpha, beta, gamma));
                    pdb.set_symmetry(
                        Symmetry::new(&spacegroup)
                            .unwrap_or_else(|| panic!("Invalid space group: \"{}\"", spacegroup)),
                    );
                }
                LexItem::Seqres(ser_num, chain_id, num_res, values) => {
                    if let Some(data) = sequence.get_mut(&chain_id) {
                        data.push((ser_num, num_res, values));
                    } else {
                        sequence.insert(chain_id, vec![(ser_num, num_res, values)]);
                    }
                }
                LexItem::Dbref(_pdb_id, chain_id, local_pos, db, db_acc, db_id, db_pos) => {
                    database_references.push((
                        chain_id,
                        DatabaseReference::new(
                            (db, db_acc, db_id),
                            SequencePosition::from_tuple(local_pos),
                            SequencePosition::from_tuple(db_pos),
                        ),
                    ));
                }
                LexItem::Seqadv(
                    _id_code,
                    chain_id,
                    res_name,
                    seq_num,
                    _insert,
                    _database,
                    _database_accession,
                    db_pos,
                    comment,
                ) => {
                    if let Some((_, db_ref)) =
                        database_references.iter_mut().find(|a| a.0 == chain_id)
                    {
                        db_ref.differences.push(SequenceDifference::new(
                            (res_name, seq_num),
                            db_pos,
                            comment,
                        ))
                    } else {
                        errors.push(PDBError::new(
                            ErrorLevel::StrictWarning,
                            "Sequence Difference Database not found",
                            &format!("For this sequence difference (chain: {}) the corresponding database definition (DBREF) was not found, make sure the DBREF is located before the SEQADV", chain_id),
                            context.clone()
                        ))
                    }
                }
                item @ LexItem::Modres(..) => modifications.push((
                    Context::Show {
                        line: format!("{:?}", item.clone()),
                    },
                    item,
                )),
                LexItem::Master(
                    num_remark,
                    num_empty,
                    _num_het,
                    _num_helix,
                    _num_sheet,
                    _num_turn,
                    _num_site,
                    num_xform,
                    num_coord,
                    _num_ter,
                    _num_connect,
                    _num_seq,
                ) => {
                    // This has to be one of the last lines so push the current model
                    if current_model.total_atom_count() > 0 {
                        pdb.add_model(current_model);
                        current_model = Model::new(0);
                    }
                    // The for now forgotten numbers will have to be added when the appropriate records are added to the parser
                    if num_remark != pdb.remark_count() {
                        errors.push(
                            PDBError::new(
                                ErrorLevel::StrictWarning,
                                "MASTER checksum failed",
                                &format!("The number of REMARKS ({}) is different then posed in the MASTER Record ({})", pdb.remark_count(), num_remark),
                                context.clone()
                            )
                        );
                    }
                    if num_empty != 0 {
                        errors.push(
                            PDBError::new(
                                ErrorLevel::LooseWarning,
                                "MASTER checksum failed",
                                &format!("The empty checksum number is not empty (value: {}) while it is defined to be empty.", num_empty),
                                context.clone()
                            )
                        );
                    }
                    let mut xform = 0;
                    if pdb.has_origx() && pdb.origx().valid() {
                        xform += 3;
                    }
                    if pdb.has_scale() && pdb.scale().valid() {
                        xform += 3;
                    }
                    for mtrix in pdb.mtrix() {
                        if mtrix.valid() {
                            xform += 3;
                        }
                    }
                    if num_xform != xform {
                        errors.push(
                            PDBError::new(
                                ErrorLevel::StrictWarning,
                                "MASTER checksum failed",
                                &format!("The number of coordinate transformation records ({}) is different then posed in the MASTER Record ({})", xform, num_xform),
                                context.clone()
                            )
                        );
                    }
                    if num_coord != pdb.total_atom_count() {
                        errors.push(
                            PDBError::new(
                                ErrorLevel::StrictWarning,
                                "MASTER checksum failed",
                                &format!("The number of Atoms (Normal + Hetero) ({}) is different then posed in the MASTER Record ({})", pdb.total_atom_count(), num_coord),
                                context.clone()
                            )
                        );
                    }
                }
                _ => (),
            }
        } else {
            errors.push(line_result.unwrap_err())
        }
    }
    if current_model.total_atom_count() > 0 {
        pdb.add_model(current_model);
    }

    for (chain_id, reference) in database_references {
        if let Some(chain) = pdb.chains_mut().find(|a| a.id() == chain_id) {
            chain.set_database_reference(reference);
        }
    }

    errors.extend(validate_seqres(&mut pdb, sequence, &context));
    errors.extend(add_modifications(&mut pdb, modifications));

    errors.extend(validate(&pdb));

    for error in &errors {
        if error.fails(level) {
            return Err(errors);
        }
    }

    Ok((pdb, errors))
}

/// Validate the SEQRES data found, if there is any
#[allow(clippy::comparison_chain)]
fn validate_seqres(
    pdb: &mut PDB,
    sequence: HashMap<char, Vec<(usize, usize, Vec<String>)>>,
    context: &Context,
) -> Vec<PDBError> {
    let mut errors = Vec::new();
    for (chain_id, data) in sequence {
        if let Some(chain) = pdb.chains_mut().find(|c| c.id() == chain_id) {
            let mut chain_sequence = Vec::new();
            let mut serial = 1;
            let mut residues = 0;
            for (ser_num, res_num, seq) in data {
                if serial != ser_num {
                    errors.push(PDBError::new(
                        ErrorLevel::StrictWarning,
                        "SEQRES serial number invalid",
                        &format!("The serial number for SEQRES chain \"{}\" with number \"{}\" does not follow sequentially from the previous row.", chain_id, ser_num),
                        context.clone()
                    ));
                }
                serial += 1;
                if residues == 0 {
                    residues = res_num;
                } else if residues != res_num {
                    errors.push(PDBError::new(
                        ErrorLevel::StrictWarning,
                        "SEQRES residue total invalid",
                        &format!("The residue total for SEQRES chain \"{}\" with number \"{}\" does not match the total on the first row for this chain.", chain_id, ser_num),
                        context.clone()
                    ));
                }
                chain_sequence.extend(seq);
            }
            if chain_sequence.len() != residues {
                errors.push(PDBError::new(
                    ErrorLevel::StrictWarning,
                    "SEQRES residue total invalid",
                    &format!("The residue total for SEQRES chain \"{}\" does not match the total residues found in the seqres records.", chain_id),
                    context.clone()
                ));
            }
            let mut offset = 1;
            if let Some(db_ref) = chain.database_reference() {
                offset = db_ref.pdb_position.start;
                for dif in &db_ref.differences {
                    if dif.database_residue.is_none() && dif.residue.1 < db_ref.pdb_position.start {
                        // If there is a residue in front of the db sequence
                        offset -= 1;
                    }
                }
                if db_ref.pdb_position.end - offset + 1 != residues {
                    errors.push(PDBError::new(
                        ErrorLevel::StrictWarning,
                        "SEQRES residue total invalid",
                        &format!("The residue total for SEQRES chain \"{}\" does not match the total residues found in the dbref record.", chain_id),
                        context.clone()
                    ));
                }
            }

            let copy = chain.clone();
            let mut chain_res = copy.residues();
            let mut next = chain_res.next();

            for (raw_index, seq) in chain_sequence.iter().enumerate() {
                let index = raw_index + offset;
                if let Some(n) = next {
                    if index == n.serial_number() {
                        if *seq != n.id() {
                            errors.push(PDBError::new(
                                ErrorLevel::StrictWarning,
                                "SEQRES residue invalid",
                                &format!("The residue index {} value \"{}\" for SEQRES chain \"{}\" does not match the residue in the chain value \"{}\".", index, chain_sequence[index], chain_id, n.id()),
                                context.clone()
                            ));
                        }
                        next = chain_res.next();
                    } else if index < n.serial_number() {
                        let three = format!("{:<3}", seq).chars().collect::<Vec<char>>();
                        chain.insert_residue(
                            index,
                            Residue::new(index, [three[0], three[1], three[2]], None)
                                .expect("Invalid characters in Residue generations"),
                        );
                    } else {
                        errors.push(PDBError::new(
                            ErrorLevel::StrictWarning,
                            "Chain residue invalid",
                            &format!("The residue index {} value \"{}\" for Chain \"{}\" is not sequentially increasing, value expected: {}.", n.serial_number(), n.id(), chain_id, index),
                            context.clone()
                        ));
                    }
                } else {
                    let three = format!("{:<3}", seq).chars().collect::<Vec<char>>();
                    chain.add_residue(
                        Residue::new(index, [three[0], three[1], three[2]], None)
                            .expect("Invalid characters in Residue generations"),
                    );
                }
            }

            if chain_sequence.len() != chain.residue_count() {
                errors.push(PDBError::new(
                    ErrorLevel::StrictWarning,
                    "SEQRES residue total invalid",
                    &format!("The residue total ({}) for SEQRES chain \"{}\" does not match the total residues found in the chain ({}).", chain_sequence.len(), chain_id, chain.residue_count()),
                    context.clone()
                ));
            } else {
                for (index, original_res) in chain.residues().enumerate() {
                    if chain_sequence[index] != original_res.id() {}
                }
            }
        }
    }
    errors
}

/// Adds all MODRES records to the Atoms
fn add_modifications(pdb: &mut PDB, modifications: Vec<(Context, LexItem)>) -> Vec<PDBError> {
    let mut errors = Vec::new();
    for (context, item) in modifications {
        match item {
            LexItem::Modres(_, res_name, chain_id, seq_num, _, std_name, comment) => {
                if let Some(chain) = pdb.chains_mut().find(|c| c.id() == chain_id) {
                    if let Some(residue) = chain
                        .residues_mut()
                        .find(|r| r.id_array() == res_name && r.serial_number() == seq_num)
                    {
                        if let Err(e) = residue.set_modification((std_name, comment)) {
                            errors.push(PDBError::new(
                                ErrorLevel::InvalidatingError,
                                "Invalid characters",
                                &e,
                                context,
                            ));
                        }
                    } else {
                        errors.push(PDBError::new(ErrorLevel::InvalidatingError, "Modified residue could not be found", "The residue presented in this MODRES record could not be found in the specified chain in the PDB file.", context))
                    }
                } else {
                    errors.push(PDBError::new(ErrorLevel::InvalidatingError, "Modified residue could not be found", "The chain presented in this MODRES record could not be found in the PDB file.", context))
                }
            }
            _ => {
                panic!("Found an invalid element in the modifications list, it is not a LexItem::Modres")
            }
        }
    }
    errors
}

/// Lex a full line. It returns a lexed item with errors if it can lex something, otherwise it will only return an error.
fn lex_line(line: String, linenumber: usize) -> Result<(LexItem, Vec<PDBError>), PDBError> {
    if line.len() > 6 {
        match &line[..6] {
            "REMARK" => lex_remark(linenumber, line),
            "ATOM  " => lex_atom(linenumber, line, false),
            "ANISOU" => Ok(lex_anisou(linenumber, line)),
            "HETATM" => lex_atom(linenumber, line, true),
            "CRYST1" => Ok(lex_cryst(linenumber, line)),
            "SCALE1" => Ok(lex_scale(linenumber, line, 0)),
            "SCALE2" => Ok(lex_scale(linenumber, line, 1)),
            "SCALE3" => Ok(lex_scale(linenumber, line, 2)),
            "ORIGX1" => Ok(lex_origx(linenumber, line, 0)),
            "ORIGX2" => Ok(lex_origx(linenumber, line, 1)),
            "ORIGX3" => Ok(lex_origx(linenumber, line, 2)),
            "MTRIX1" => Ok(lex_mtrix(linenumber, line, 0)),
            "MTRIX2" => Ok(lex_mtrix(linenumber, line, 1)),
            "MTRIX3" => Ok(lex_mtrix(linenumber, line, 2)),
            "MODEL " => Ok(lex_model(linenumber, line)),
            "MASTER" => Ok(lex_master(linenumber, line)),
            "DBREF " => Ok(lex_dbref(linenumber, line)),
            "SEQRES" => Ok(lex_seqres(linenumber, line)),
            "SEQADV" => Ok(lex_seqadv(linenumber, line)),
            "MODRES" => Ok(lex_modres(linenumber, line)),
            "ENDMDL" => Ok((LexItem::EndModel(), Vec::new())),
            "TER   " => Ok((LexItem::TER(), Vec::new())),
            "END   " => Ok((LexItem::End(), Vec::new())),
            _ => Err(PDBError::new(ErrorLevel::GeneralWarning, "Could not recognise tag.", "Could not parse the tag above, it is possible that it is valid PDB but just not supported right now.",Context::full_line(linenumber, &line))),
        }
    } else if line.len() > 2 {
        match &line[..3] {
            "TER" => Ok((LexItem::TER(), Vec::new())),
            "END" => Ok((LexItem::End(), Vec::new())),
            _ => Err(PDBError::new(ErrorLevel::GeneralWarning, "Could not recognise tag.", "Could not parse the tag above, it is possible that it is valid PDB but just not supported right now.",Context::full_line(linenumber, &line))),
        }
    } else if !line.is_empty() {
        Err(PDBError::new(ErrorLevel::GeneralWarning, "Could not recognise tag.", "Could not parse the tag above, it is possible that it is valid PDB but just not supported right now.",Context::full_line(linenumber, &line)))
    } else {
        Ok((LexItem::Empty(), Vec::new()))
    }
}

/// Lex a REMARK
/// ## Fails
/// It fails on incorrect numbers for the remark-type-number
fn lex_remark(linenumber: usize, line: String) -> Result<(LexItem, Vec<PDBError>), PDBError> {
    let mut errors = Vec::new();
    let number = match parse_number(
        Context::line(linenumber, &line, 7, 3),
        &line.chars().collect::<Vec<char>>()[7..10],
    ) {
        Ok(n) => n,
        Err(e) => {
            errors.push(e);
            0
        }
    };
    if !reference_tables::valid_remark_type_number(number) {
        errors.push(PDBError::new(
            ErrorLevel::StrictWarning,
            "Remark type number invalid",
            "The remark-type-number is not valid, see wwPDB v3.30 for all valid numbers.",
            Context::line(linenumber, &line, 7, 3),
        ));
    }
    Ok((
        LexItem::Remark(
            number,
            if line.len() > 11 {
                if line.len() - 11 > 70 {
                    return Err(PDBError::new(
                        ErrorLevel::LooseWarning,
                        "Remark too long",
                        "The REMARK is too long, the max is 70 characters.",
                        Context::line(linenumber, &line, 11, line.len() - 11),
                    ));
                }
                line[11..].to_string()
            } else {
                "".to_string()
            },
        ),
        errors,
    ))
}

/// Lex a MODEL
/// ## Fails
/// It fails on incorrect numbers for the serial number
fn lex_model(linenumber: usize, line: String) -> (LexItem, Vec<PDBError>) {
    let mut errors = Vec::new();
    let number = match parse_number(
        Context::line(linenumber, &line, 6, line.len() - 6),
        &line.chars().collect::<Vec<char>>()[6..]
            .iter()
            .collect::<String>()
            .trim()
            .chars()
            .collect::<Vec<char>>()[..],
    ) {
        Ok(n) => n,
        Err(e) => {
            errors.push(e);
            0
        }
    };
    (LexItem::Model(number), errors)
}

/// Lex an ATOM
/// ## Fails
/// It fails on incorrect numbers in the line
fn lex_atom(
    linenumber: usize,
    line: String,
    hetero: bool,
) -> Result<(LexItem, Vec<PDBError>), PDBError> {
    let mut errors = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    if chars.len() < 54 {
        return Err(PDBError::new(
            ErrorLevel::BreakingError,
            "Atom line too short",
            "This line is too short to contain all necessary elements (up to `z` at least).",
            Context::full_line(linenumber, &line),
        ));
    };
    let mut check = |item| match item {
        Ok(t) => t,
        Err(e) => {
            errors.push(e);
            0.0
        }
    };
    let x = check(parse_number(
        Context::line(linenumber, &line, 30, 8),
        &chars[30..38],
    ));
    let y = check(parse_number(
        Context::line(linenumber, &line, 38, 8),
        &chars[38..46],
    ));
    let z = check(parse_number(
        Context::line(linenumber, &line, 46, 8),
        &chars[46..54],
    ));
    let mut occupancy = 1.0;
    if chars.len() >= 60 {
        occupancy = check(parse_number(
            Context::line(linenumber, &line, 54, 6),
            &chars[54..60],
        ));
    }
    let mut b_factor = 0.0;
    if chars.len() >= 66 {
        b_factor = check(parse_number(
            Context::line(linenumber, &line, 60, 6),
            &chars[60..66],
        ));
    }

    let (
        (
            serial_number,
            atom_name,
            alternate_location,
            residue_name,
            chain_id,
            residue_serial_number,
            insertion,
            segment_id,
            element,
            charge,
        ),
        basic_errors,
    ) = lex_atom_basics(linenumber, line);
    errors.extend(basic_errors);

    Ok((
        LexItem::Atom(
            hetero,
            serial_number,
            atom_name,
            alternate_location,
            residue_name,
            chain_id,
            residue_serial_number,
            insertion,
            x,
            y,
            z,
            occupancy,
            b_factor,
            segment_id,
            element,
            charge,
        ),
        errors,
    ))
}

/// Lex an ANISOU
/// ## Fails
/// It fails on incorrect numbers in the line
fn lex_anisou(linenumber: usize, line: String) -> (LexItem, Vec<PDBError>) {
    let mut errors = Vec::new();
    let mut check = |item| match item {
        Ok(t) => t,
        Err(e) => {
            errors.push(e);
            0
        }
    };
    let chars: Vec<char> = line.chars().collect();
    let ai: isize = check(parse_number(
        Context::line(linenumber, &line, 28, 7),
        &chars[28..35],
    ));
    let bi: isize = check(parse_number(
        Context::line(linenumber, &line, 35, 7),
        &chars[35..42],
    ));
    let ci: isize = check(parse_number(
        Context::line(linenumber, &line, 42, 7),
        &chars[42..49],
    ));
    let di: isize = check(parse_number(
        Context::line(linenumber, &line, 49, 7),
        &chars[49..56],
    ));
    let ei: isize = check(parse_number(
        Context::line(linenumber, &line, 56, 7),
        &chars[56..63],
    ));
    let fi: isize = check(parse_number(
        Context::line(linenumber, &line, 63, 7),
        &chars[63..70],
    ));
    #[allow(clippy::cast_precision_loss)]
    let factors = [
        [
            (ai as f64) / 10000.0,
            (bi as f64) / 10000.0,
            (ci as f64) / 10000.0,
        ],
        [
            (di as f64) / 10000.0,
            (ei as f64) / 10000.0,
            (fi as f64) / 10000.0,
        ],
    ];

    let (
        (
            serial_number,
            atom_name,
            alternate_location,
            residue_name,
            chain_id,
            residue_serial_number,
            insertion,
            segment_id,
            element,
            charge,
        ),
        basic_errors,
    ) = lex_atom_basics(linenumber, line);
    errors.extend(basic_errors);

    (
        LexItem::Anisou(
            serial_number,
            atom_name,
            alternate_location,
            residue_name,
            chain_id,
            residue_serial_number,
            insertion,
            factors,
            segment_id,
            element,
            charge,
        ),
        errors,
    )
}

/// Lex the basic structure of the ATOM/HETATM/ANISOU Records, to minimise code duplication
#[allow(clippy::type_complexity)]
fn lex_atom_basics(
    linenumber: usize,
    line: String,
) -> (
    (
        usize,
        [char; 4],
        char,
        [char; 3],
        char,
        usize,
        char,
        [char; 4],
        [char; 2],
        isize,
    ),
    Vec<PDBError>,
) {
    let mut errors = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut check_usize = |item| match item {
        Ok(t) => t,
        Err(e) => {
            errors.push(e);
            0
        }
    };
    let serial_number = check_usize(parse_number(
        Context::line(linenumber, &line, 7, 4),
        &chars[7..11],
    ));
    let atom_name = [chars[12], chars[13], chars[14], chars[15]];
    let alternate_location = chars[16];
    let residue_name = [chars[17], chars[18], chars[19]];
    let chain_id = chars[21];
    let residue_serial_number = check_usize(parse_number(
        Context::line(linenumber, &line, 22, 4),
        &chars[22..26],
    ));
    let insertion = chars[26];
    let mut segment_id = [' ', ' ', ' ', ' '];
    if chars.len() >= 75 {
        segment_id = [chars[72], chars[73], chars[74], chars[75]];
    }
    let mut element = [' ', ' '];
    if chars.len() >= 77 {
        element = [chars[76], chars[77]];
    }
    let mut charge = 0;
    #[allow(clippy::unwrap_used)]
    if chars.len() >= 79 && !(chars[78] == ' ' && chars[79] == ' ') {
        if !chars[78].is_ascii_digit() {
            errors.push(PDBError::new(
                ErrorLevel::InvalidatingError,
                "Atom charge is not correct",
                "The charge is not numeric, it is defined to be [0-9][+-], so two characters in total.",
                Context::line(linenumber, &line, 78, 1),
            ));
        } else if chars[79] != '-' && chars[79] != '+' {
            errors.push(PDBError::new(
                ErrorLevel::InvalidatingError,
                "Atom charge is not correct",
                "The charge is not properly signed, it is defined to be [0-9][+-], so two characters in total.",
                Context::line(linenumber, &line, 79, 1),
            ));
        } else {
            charge = isize::try_from(chars[78].to_digit(10).unwrap()).unwrap();
            if chars[79] == '-' {
                charge *= -1;
            }
        }
    }

    (
        (
            serial_number,
            atom_name,
            alternate_location,
            residue_name,
            chain_id,
            residue_serial_number,
            insertion,
            segment_id,
            element,
            charge,
        ),
        errors,
    )
}

/// Lex a CRYST1
/// ## Fails
/// It fails on incorrect numbers in the line
fn lex_cryst(linenumber: usize, line: String) -> (LexItem, Vec<PDBError>) {
    let mut errors = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut check = |item| match item {
        Ok(t) => t,
        Err(e) => {
            errors.push(e);
            0.0
        }
    };
    let a = check(parse_number(
        Context::line(linenumber, &line, 6, 9),
        &chars[6..15],
    ));
    let b = check(parse_number(
        Context::line(linenumber, &line, 15, 9),
        &chars[15..24],
    ));
    let c = check(parse_number(
        Context::line(linenumber, &line, 24, 9),
        &chars[24..33],
    ));
    let alpha = check(parse_number(
        Context::line(linenumber, &line, 33, 7),
        &chars[33..40],
    ));
    let beta = check(parse_number(
        Context::line(linenumber, &line, 40, 7),
        &chars[40..47],
    ));
    let gamma = check(parse_number(
        Context::line(linenumber, &line, 47, 7),
        &chars[47..54],
    ));
    let spacegroup = chars[55..std::cmp::min(66, chars.len())]
        .iter()
        .collect::<String>();
    let mut z = 1;
    if chars.len() > 66 {
        z = match parse_number(
            Context::line(linenumber, &line, 66, line.len() - 66),
            &chars[66..],
        ) {
            Ok(value) => value,
            Err(error) => {
                errors.push(error);
                0
            }
        };
    }

    (
        LexItem::Crystal(a, b, c, alpha, beta, gamma, spacegroup, z),
        errors,
    )
}

/// Lex an SCALEn (where `n` is given)
/// ## Fails
/// It fails on incorrect numbers in the line
fn lex_scale(linenumber: usize, line: String, row: usize) -> (LexItem, Vec<PDBError>) {
    let (data, errors) = lex_transformation(linenumber, line);

    (LexItem::Scale(row, data), errors)
}

/// Lex an ORIGXn (where `n` is given)
/// ## Fails
/// It fails on incorrect numbers in the line
fn lex_origx(linenumber: usize, line: String, row: usize) -> (LexItem, Vec<PDBError>) {
    let (data, errors) = lex_transformation(linenumber, line);

    (LexItem::OrigX(row, data), errors)
}

/// Lex an MTRIXn (where `n` is given)
/// ## Fails
/// It fails on incorrect numbers in the line
fn lex_mtrix(linenumber: usize, line: String, row: usize) -> (LexItem, Vec<PDBError>) {
    let mut errors = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut check = |item| match item {
        Ok(t) => t,
        Err(e) => {
            errors.push(e);
            0
        }
    };
    let ser = check(parse_number(
        Context::line(linenumber, &line, 7, 4),
        &chars[7..10],
    ));
    let (data, transformation_errors) = lex_transformation(linenumber, line);
    errors.extend(transformation_errors);

    let mut given = false;
    if chars.len() >= 60 {
        given = chars[59] == '1';
    }

    (LexItem::MtriX(row, ser, data, given), errors)
}

/// Lexes the general structure of a transformation record (ORIGXn, SCALEn, MTRIXn)
fn lex_transformation(linenumber: usize, line: String) -> ([f64; 4], Vec<PDBError>) {
    let mut errors = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut check = |item| match item {
        Ok(t) => t,
        Err(e) => {
            errors.push(e);
            0.0
        }
    };
    let a = check(parse_number(
        Context::line(linenumber, &line, 10, 10),
        &chars[10..20],
    ));
    let b = check(parse_number(
        Context::line(linenumber, &line, 20, 10),
        &chars[20..30],
    ));
    let c = check(parse_number(
        Context::line(linenumber, &line, 30, 10),
        &chars[30..40],
    ));
    let d = check(parse_number(
        Context::line(linenumber, &line, 45, 10),
        &chars[45..55],
    ));

    ([a, b, c, d], errors)
}

/// Lex a MASTER
/// ## Fails
/// It fails on incorrect numbers in the line
fn lex_master(linenumber: usize, line: String) -> (LexItem, Vec<PDBError>) {
    let mut errors = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut check = |item| match item {
        Ok(t) => t,
        Err(e) => {
            errors.push(e);
            0
        }
    };
    let num_remark = check(parse_number(
        Context::line(linenumber, &line, 10, 5),
        &chars[10..15],
    ));
    let num_empty = check(parse_number(
        Context::line(linenumber, &line, 15, 5),
        &chars[15..20],
    ));
    let num_het = check(parse_number(
        Context::line(linenumber, &line, 20, 5),
        &chars[20..25],
    ));
    let num_helix = check(parse_number(
        Context::line(linenumber, &line, 25, 5),
        &chars[25..30],
    ));
    let num_sheet = check(parse_number(
        Context::line(linenumber, &line, 30, 5),
        &chars[30..35],
    ));
    let num_turn = check(parse_number(
        Context::line(linenumber, &line, 35, 5),
        &chars[35..40],
    ));
    let num_site = check(parse_number(
        Context::line(linenumber, &line, 40, 5),
        &chars[40..45],
    ));
    let num_xform = check(parse_number(
        Context::line(linenumber, &line, 45, 5),
        &chars[45..50],
    ));
    let num_coord = check(parse_number(
        Context::line(linenumber, &line, 50, 5),
        &chars[50..55],
    ));
    let num_ter = check(parse_number(
        Context::line(linenumber, &line, 55, 5),
        &chars[55..60],
    ));
    let num_connect = check(parse_number(
        Context::line(linenumber, &line, 60, 5),
        &chars[60..65],
    ));
    let num_seq = check(parse_number(
        Context::line(linenumber, &line, 65, 5),
        &chars[65..70],
    ));

    (
        LexItem::Master(
            num_remark,
            num_empty,
            num_het,
            num_helix,
            num_sheet,
            num_turn,
            num_site,
            num_xform,
            num_coord,
            num_ter,
            num_connect,
            num_seq,
        ),
        errors,
    )
}

/// Lexes a SEQRES record
fn lex_seqres(linenumber: usize, line: String) -> (LexItem, Vec<PDBError>) {
    let mut errors = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut check = |item| match item {
        Ok(t) => t,
        Err(e) => {
            errors.push(e);
            0
        }
    };
    let ser_num = check(parse_number(
        Context::line(linenumber, &line, 7, 3),
        &chars[7..10],
    ));
    let chain_id = chars[11];
    let num_res = check(parse_number(
        Context::line(linenumber, &line, 13, 4),
        &chars[13..17],
    ));
    let mut values = Vec::new();
    let mut index = 19;
    let max = cmp::min(chars.len(), 71);
    while index + 3 < max {
        let seq = chars[index..index + 3].iter().collect::<String>();
        if seq == "   " {
            break;
        }
        values.push(seq);
        index += 4;
    }
    (LexItem::Seqres(ser_num, chain_id, num_res, values), errors)
}

/// Lexes a DBREF record
fn lex_dbref(linenumber: usize, line: String) -> (LexItem, Vec<PDBError>) {
    let mut errors = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut check = |item| match item {
        Ok(t) => t,
        Err(e) => {
            errors.push(e);
            0
        }
    };
    let id_code = [chars[7], chars[8], chars[9], chars[10]];
    let chain_id = chars[12];
    let seq_begin = check(parse_number(
        Context::line(linenumber, &line, 14, 4),
        &chars[14..18],
    ));
    let insert_begin = chars[18];
    let seq_end = check(parse_number(
        Context::line(linenumber, &line, 21, 4),
        &chars[21..24],
    ));
    let insert_end = chars[24];
    let database = chars[26..32].iter().collect::<String>().trim().to_string();
    let database_accession = chars[33..41].iter().collect::<String>().trim().to_string();
    let database_id_code = chars[42..54].iter().collect::<String>().trim().to_string();
    let database_seq_begin = check(parse_number(
        Context::line(linenumber, &line, 55, 5),
        &chars[55..60],
    ));
    let database_insert_begin = chars[60];
    let database_seq_end = check(parse_number(
        Context::line(linenumber, &line, 62, 5),
        &chars[62..67],
    ));
    let database_insert_end = chars[67];

    (
        LexItem::Dbref(
            id_code,
            chain_id,
            (seq_begin, insert_begin, seq_end, insert_end),
            database,
            database_accession,
            database_id_code,
            (
                database_seq_begin,
                database_insert_begin,
                database_seq_end,
                database_insert_end,
            ),
        ),
        errors,
    )
}

/// Lexes a SEQADV record
fn lex_seqadv(linenumber: usize, line: String) -> (LexItem, Vec<PDBError>) {
    let mut errors = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut check = |item| match item {
        Ok(t) => t,
        Err(e) => {
            errors.push(e);
            0
        }
    };
    let id_code = [chars[7], chars[8], chars[9], chars[10]];
    let res_name = [chars[12], chars[13], chars[14]];
    let chain_id = chars[16];
    let seq_num = check(parse_number(
        Context::line(linenumber, &line, 18, 4),
        &chars[18..22],
    ));
    let insert = chars[22];
    let database = chars[24..28].iter().collect::<String>().trim().to_string();
    let database_accession = chars[29..38].iter().collect::<String>().trim().to_string();

    let mut db_pos = None;
    if !chars[39..48].iter().all(|c| *c == ' ') {
        let db_res_name = [chars[39], chars[40], chars[41]];
        let db_seq_num = check(parse_number(
            Context::line(linenumber, &line, 43, 5),
            &chars[43..48],
        ));
        db_pos = Some((db_res_name, db_seq_num));
    }
    let comment = chars[49..].iter().collect::<String>().trim().to_string();

    (
        LexItem::Seqadv(
            id_code,
            chain_id,
            res_name,
            seq_num,
            insert,
            database,
            database_accession,
            db_pos,
            comment,
        ),
        errors,
    )
}

/// Lexes a MODRES record
fn lex_modres(linenumber: usize, line: String) -> (LexItem, Vec<PDBError>) {
    let mut errors = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut check = |item| match item {
        Ok(t) => t,
        Err(e) => {
            errors.push(e);
            0
        }
    };
    let id = [chars[7], chars[8], chars[9], chars[10]];
    let res_name = [chars[12], chars[13], chars[14]];
    let chain_id = chars[16];
    let seq_num = check(parse_number(
        Context::line(linenumber, &line, 18, 4),
        &chars[18..22],
    ));
    let insert = chars[22];
    let std_res = [chars[24], chars[25], chars[26]];
    let comment = chars[29..].iter().collect::<String>().trim().to_string();

    (
        LexItem::Modres(id, res_name, chain_id, seq_num, insert, std_res, comment),
        errors,
    )
}

/// Parse a number, generic for anything that can be parsed using FromStr
fn parse_number<T: FromStr>(context: Context, input: &[char]) -> Result<T, PDBError> {
    let string = input
        .iter()
        .collect::<String>()
        .split_whitespace()
        .collect::<String>();
    match string.parse::<T>() {
        Ok(v) => Ok(v),
        Err(_) => Err(PDBError::new(
            ErrorLevel::InvalidatingError,
            "Not a number",
            "The text presented is not a number of the right kind.",
            context,
        )),
    }
}
