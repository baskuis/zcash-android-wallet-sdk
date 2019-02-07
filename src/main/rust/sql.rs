use failure::{format_err, Error};
use ff::{PrimeField, PrimeFieldRepr};
use pairing::bls12_381::Bls12;
use protobuf::parse_from_bytes;
use rusqlite::{types::ToSql, Connection, NO_PARAMS};
use sapling_crypto::{
    jubjub::fs::{Fs, FsRepr},
    primitives::{Diversifier, Note, PaymentAddress},
};
use zcash_client_backend::{
    constants::{HRP_SAPLING_EXTENDED_FULL_VIEWING_KEY_TEST, HRP_SAPLING_PAYMENT_ADDRESS_TEST},
    encoding::{
        decode_extended_full_viewing_key, encode_extended_full_viewing_key, encode_payment_address,
    },
    note_encryption::Memo,
    proto::compact_formats::CompactBlock,
    prover::TxProver,
    transaction::Builder,
    welding_rig::scan_block,
};
use zcash_primitives::{
    merkle_tree::{CommitmentTree, IncrementalWitness},
    transaction::components::Amount,
    JUBJUB,
};
use zip32::{ExtendedFullViewingKey, ExtendedSpendingKey};

const ANCHOR_OFFSET: u32 = 10;

fn address_from_extfvk(extfvk: &ExtendedFullViewingKey) -> String {
    let addr = extfvk.default_address().unwrap().1;
    encode_payment_address(HRP_SAPLING_PAYMENT_ADDRESS_TEST, &addr)
}

pub fn init_data_database(db_data: &str) -> rusqlite::Result<()> {
    let data = Connection::open(db_data)?;
    data.execute(
        "CREATE TABLE IF NOT EXISTS accounts (
            account INTEGER PRIMARY KEY,
            extfvk TEXT NOT NULL,
            address TEXT NOT NULL
        )",
        NO_PARAMS,
    )?;
    data.execute(
        "CREATE TABLE IF NOT EXISTS blocks (
            height INTEGER PRIMARY KEY,
            time INTEGER NOT NULL,
            sapling_tree BLOB NOT NULL
        )",
        NO_PARAMS,
    )?;
    data.execute(
        "CREATE TABLE IF NOT EXISTS transactions (
            id_tx INTEGER PRIMARY KEY,
            txid BLOB NOT NULL UNIQUE,
            block INTEGER,
            tx_index INTEGER,
            raw BLOB,
            FOREIGN KEY (block) REFERENCES blocks(height)
        )",
        NO_PARAMS,
    )?;
    data.execute(
        "CREATE TABLE IF NOT EXISTS received_notes (
            id_note INTEGER PRIMARY KEY,
            tx INTEGER NOT NULL,
            output_index INTEGER NOT NULL,
            account INTEGER NOT NULL,
            diversifier BLOB NOT NULL,
            value INTEGER NOT NULL,
            rcm BLOB NOT NULL,
            nf BLOB NOT NULL UNIQUE,
            is_change BOOLEAN NOT NULL,
            memo BLOB,
            spent INTEGER,
            FOREIGN KEY (tx) REFERENCES transactions(id_tx),
            FOREIGN KEY (account) REFERENCES accounts(account),
            FOREIGN KEY (spent) REFERENCES transactions(id_tx),
            CONSTRAINT tx_output UNIQUE (tx, output_index)
        )",
        NO_PARAMS,
    )?;
    data.execute(
        "CREATE TABLE IF NOT EXISTS sapling_witnesses (
            id_witness INTEGER PRIMARY KEY,
            note INTEGER NOT NULL,
            block INTEGER NOT NULL,
            witness BLOB NOT NULL,
            FOREIGN KEY (note) REFERENCES received_notes(id_note),
            FOREIGN KEY (block) REFERENCES blocks(height),
            CONSTRAINT witness_height UNIQUE (note, block)
        )",
        NO_PARAMS,
    )?;
    data.execute(
        "CREATE TABLE IF NOT EXISTS sent_notes (
            id_note INTEGER PRIMARY KEY,
            tx INTEGER NOT NULL,
            output_index INTEGER NOT NULL,
            from_account INTEGER NOT NULL,
            address TEXT NOT NULL,
            value INTEGER NOT NULL,
            memo BLOB,
            FOREIGN KEY (tx) REFERENCES transactions(id_tx),
            FOREIGN KEY (from_account) REFERENCES accounts(account),
            CONSTRAINT tx_output UNIQUE (tx, output_index)
        )",
        NO_PARAMS,
    )?;
    Ok(())
}

pub fn init_accounts_table(db_data: &str, extfvks: &[ExtendedFullViewingKey]) -> Result<(), Error> {
    let data = Connection::open(db_data)?;

    let mut empty_check = data.prepare("SELECT * FROM accounts LIMIT 1")?;
    if empty_check.exists(NO_PARAMS)? {
        return Err(format_err!("accounts table is not empty"));
    }

    // Insert accounts atomically
    data.execute("BEGIN IMMEDIATE", NO_PARAMS)?;
    for (account, extfvk) in extfvks.iter().enumerate() {
        let address = address_from_extfvk(extfvk);
        let extfvk =
            encode_extended_full_viewing_key(HRP_SAPLING_EXTENDED_FULL_VIEWING_KEY_TEST, extfvk);
        data.execute(
            "INSERT INTO accounts (account, extfvk, address)
            VALUES (?, ?, ?)",
            &[
                (account as u32).to_sql()?,
                extfvk.to_sql()?,
                address.to_sql()?,
            ],
        )?;
    }
    data.execute("COMMIT", NO_PARAMS)?;

    Ok(())
}

pub fn init_blocks_table(
    db_data: &str,
    height: i32,
    time: u32,
    sapling_tree: &[u8],
) -> Result<(), Error> {
    let data = Connection::open(db_data)?;

    let mut empty_check = data.prepare("SELECT * FROM blocks LIMIT 1")?;
    if empty_check.exists(NO_PARAMS)? {
        return Err(format_err!("blocks table is not empty"));
    }

    data.execute(
        "INSERT INTO blocks (height, time, sapling_tree)
        VALUES (?, ?, ?)",
        &[height.to_sql()?, time.to_sql()?, sapling_tree.to_sql()?],
    )?;

    Ok(())
}

struct CompactBlockRow {
    height: i32,
    data: Vec<u8>,
}

#[derive(Clone)]
struct WitnessRow {
    id_note: i64,
    witness: IncrementalWitness,
}

pub fn get_address(db_data: &str, account: u32) -> Result<String, Error> {
    let data = Connection::open(db_data)?;

    let addr = data.query_row(
        "SELECT address FROM accounts
        WHERE account = ?",
        &[account],
        |row| row.get(0),
    )?;

    Ok(addr)
}

pub fn get_balance(db_data: &str, account: u32) -> Result<Amount, Error> {
    let data = Connection::open(db_data)?;

    let balance = data.query_row(
        "SELECT SUM(value) FROM received_notes
        WHERE account = ? AND spent IS NULL",
        &[account],
        |row| row.get_checked(0).unwrap_or(0),
    )?;

    Ok(Amount(balance))
}

/// Scans new blocks added to the cache for any transactions received by the
/// tracked accounts.
///
/// Assumes that the caller is handling rollbacks.
pub fn scan_cached_blocks(db_cache: &str, db_data: &str) -> Result<(), Error> {
    let cache = Connection::open(db_cache)?;
    let data = Connection::open(db_data)?;

    // Recall where we synced up to previously.
    // If we have never synced, use 0 to select all cached CompactBlocks.
    let mut last_height =
        data.query_row(
            "SELECT MAX(height) FROM blocks",
            NO_PARAMS,
            |row| match row.get_checked(0) {
                Ok(h) => h,
                Err(_) => 0,
            },
        )?;

    // Prepare necessary SQL statements
    let mut stmt_blocks = cache
        .prepare("SELECT height, data FROM compactblocks WHERE height > ? ORDER BY height ASC")?;
    let mut stmt_fetch_tree = data.prepare("SELECT sapling_tree FROM blocks WHERE height = ?")?;
    let mut stmt_fetch_witnesses =
        data.prepare("SELECT note, witness FROM sapling_witnesses WHERE block = ?")?;
    let mut stmt_fetch_nullifiers =
        data.prepare("SELECT id_note, nf, account FROM received_notes WHERE spent IS NULL")?;
    let mut stmt_insert_block = data.prepare(
        "INSERT INTO blocks (height, time, sapling_tree)
        VALUES (?, ?, ?)",
    )?;
    let mut stmt_update_tx = data.prepare(
        "UPDATE transactions
        SET block = ?, tx_index = ? WHERE txid = ?",
    )?;
    let mut stmt_insert_tx = data.prepare(
        "INSERT INTO transactions (txid, block, tx_index)
        VALUES (?, ?, ?)",
    )?;
    let mut stmt_select_tx = data.prepare("SELECT id_tx FROM transactions WHERE txid = ?")?;
    let mut stmt_mark_spent_note =
        data.prepare("UPDATE received_notes SET spent = ? WHERE nf = ?")?;
    let mut stmt_insert_note = data.prepare(
        "INSERT INTO received_notes (tx, output_index, account, diversifier, value, rcm, nf, is_change)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )?;
    let mut stmt_insert_witness = data.prepare(
        "INSERT INTO sapling_witnesses (note, block, witness)
        VALUES (?, ?, ?)",
    )?;
    let mut stmt_prune_witnesses = data.prepare("DELETE FROM sapling_witnesses WHERE block < ?")?;

    // Fetch the CompactBlocks we need to scan
    let rows = stmt_blocks.query_map(&[last_height], |row| CompactBlockRow {
        height: row.get(0),
        data: row.get(1),
    })?;

    // Fetch the ExtendedFullViewingKeys we are tracking
    let mut stmt_fetch_accounts =
        data.prepare("SELECT extfvk FROM accounts ORDER BY account ASC")?;
    let extfvks = stmt_fetch_accounts.query_map(NO_PARAMS, |row| {
        let extfvk: String = row.get(0);
        decode_extended_full_viewing_key(HRP_SAPLING_EXTENDED_FULL_VIEWING_KEY_TEST, &extfvk)
    })?;
    // Raise SQL errors from the query, and IO errors from parsing.
    let extfvks: Vec<_> = extfvks.collect::<Result<Result<_, _>, _>>()??;

    // Get the most recent CommitmentTree
    let mut tree = match stmt_fetch_tree.query_row(&[last_height], |row| match row.get_checked(0) {
        Ok(data) => {
            let data: Vec<_> = data;
            CommitmentTree::read(&data[..]).unwrap()
        }
        Err(_) => CommitmentTree::new(),
    }) {
        Ok(tree) => tree,
        Err(_) => CommitmentTree::new(),
    };

    // Get most recent incremental witnesses for the notes we are tracking
    let witnesses = stmt_fetch_witnesses.query_map(&[last_height], |row| {
        let data: Vec<_> = row.get(1);
        WitnessRow {
            id_note: row.get(0),
            witness: IncrementalWitness::read(&data[..]).unwrap(),
        }
    })?;
    let mut witnesses: Vec<_> = witnesses.collect::<Result<_, _>>()?;

    // Get the nullifiers for the notes we are tracking
    let nullifiers = stmt_fetch_nullifiers.query_map(NO_PARAMS, |row| {
        let nf: Vec<_> = row.get(1);
        let account: i64 = row.get(2);
        (nf, account as usize)
    })?;
    let mut nullifiers: Vec<_> = nullifiers.collect::<Result<_, _>>()?;

    for row in rows {
        let row = row?;

        // Start an SQL transaction for this block.
        data.execute("BEGIN IMMEDIATE", NO_PARAMS)?;

        // Scanned blocks MUST be height-ascending, but they might not be height-sequential
        // (e.g. if blocks that don't contain Sapling data are skipped). Note that this
        // introduces a risk that a block containing Sapling data is skipped; it is up to
        // the caller to ensure this does not happen.
        if row.height <= last_height {
            return Err(format_err!(
                "Expected height of next CompactBlock to be at least {}, but was {}",
                last_height + 1,
                row.height
            ));
        }
        last_height = row.height;

        let block: CompactBlock = parse_from_bytes(&row.data)?;
        let block_time = block.time;

        let txs = {
            let nf_refs: Vec<_> = nullifiers.iter().map(|(nf, acc)| (&nf[..], *acc)).collect();
            let mut witness_refs: Vec<_> = witnesses.iter_mut().map(|w| &mut w.witness).collect();
            scan_block(
                block,
                &extfvks[..],
                &nf_refs,
                &mut tree,
                &mut witness_refs[..],
            )
        };

        // Insert the block into the database.
        let mut encoded_tree = Vec::new();
        tree.write(&mut encoded_tree).unwrap();
        stmt_insert_block.execute(&[
            row.height.to_sql()?,
            block_time.to_sql()?,
            encoded_tree.to_sql()?,
        ])?;

        for (tx, new_witnesses) in txs {
            // First try update an existing transaction in the database.
            let txid = tx.txid.0.to_vec();
            let tx_row = if stmt_update_tx.execute(&[
                row.height.to_sql()?,
                (tx.index as i64).to_sql()?,
                txid.to_sql()?,
            ])? == 0
            {
                // It isn't there, so insert our transaction into the database.
                stmt_insert_tx.execute(&[
                    txid.to_sql()?,
                    row.height.to_sql()?,
                    (tx.index as i64).to_sql()?,
                ])?;
                data.last_insert_rowid()
            } else {
                // It was there, so grab its row number.
                stmt_select_tx.query_row(&[txid], |row| row.get(0))?
            };

            // Mark notes as spent and remove them from the scanning cache
            for spend in &tx.shielded_spends {
                stmt_mark_spent_note.execute(&[tx_row.to_sql()?, spend.nf.to_sql()?])?;
            }
            nullifiers = nullifiers
                .into_iter()
                .filter(|(nf, _acc)| {
                    tx.shielded_spends
                        .iter()
                        .find(|spend| &spend.nf == nf)
                        .is_none()
                })
                .collect();

            for (output, witness) in tx
                .shielded_outputs
                .into_iter()
                .zip(new_witnesses.into_iter())
            {
                let mut rcm = [0; 32];
                output.note.r.into_repr().write_le(&mut rcm[..])?;
                let nf = output.note.nf(
                    &extfvks[output.account].fvk.vk,
                    witness.position() as u64,
                    &JUBJUB,
                );

                // Insert received note into the database.
                // Assumptions:
                // - A transaction will not contain more than 2^63 shielded outputs.
                // - A note value will never exceed 2^63 zatoshis.
                stmt_insert_note.execute(&[
                    tx_row.to_sql()?,
                    (output.index as i64).to_sql()?,
                    (output.account as i64).to_sql()?,
                    output.to.diversifier.0.to_sql()?,
                    (output.note.value as i64).to_sql()?,
                    rcm.to_sql()?,
                    nf.to_sql()?,
                    output.is_change.to_sql()?,
                ])?;
                let note_row = data.last_insert_rowid();

                // Save witness for note.
                witnesses.push(WitnessRow {
                    id_note: note_row,
                    witness,
                });

                // Cache nullifier for note (to detect subsequent spends in this scan).
                nullifiers.push((nf, output.account));
            }
        }

        // Insert current witnesses into the database.
        let mut encoded = Vec::new();
        for witness_row in witnesses.iter() {
            encoded.clear();
            witness_row.witness.write(&mut encoded).unwrap();
            stmt_insert_witness.execute(&[
                witness_row.id_note.to_sql()?,
                last_height.to_sql()?,
                encoded.to_sql()?,
            ])?;
        }

        // Prune the stored witnesses (we only expect rollbacks of at most 100 blocks).
        stmt_prune_witnesses.execute(&[last_height - 100])?;

        // Commit the SQL transaction, writing this block's data atomically.
        data.execute("COMMIT", NO_PARAMS)?;
    }

    Ok(())
}

struct SelectedNoteRow {
    diversifier: Diversifier,
    note: Note<Bls12>,
    witness: IncrementalWitness,
}

/// Creates a transaction paying the specified address.
pub fn send_to_address(
    db_data: &str,
    consensus_branch_id: u32,
    prover: impl TxProver,
    account: u32,
    extsk: &ExtendedSpendingKey,
    to: &PaymentAddress<Bls12>,
    value: Amount,
    memo: Option<Memo>,
) -> Result<i64, Error> {
    let data = Connection::open(db_data)?;

    // Check that the ExtendedSpendingKey we have been given corresponds to the
    // ExtendedFullViewingKey for the account we are spending from.
    let extfvk = ExtendedFullViewingKey::from(extsk);
    if !data
        .prepare("SELECT * FROM accounts WHERE account = ? AND extfvk = ?")?
        .exists(&[
            account.to_sql()?,
            encode_extended_full_viewing_key(HRP_SAPLING_EXTENDED_FULL_VIEWING_KEY_TEST, &extfvk)
                .to_sql()?,
        ])?
    {
        return Err(format_err!(
            "Incorrect ExtendedSpendingKey for account {}",
            account
        ));
    }
    let ovk = extfvk.fvk.ovk;

    // Target the next block, assuming we are up-to-date.
    let height = data.query_row_and_then("SELECT MAX(height) FROM blocks", NO_PARAMS, |row| {
        let ret: Result<u32, _> = row.get_checked(0);
        ret
    })? + 1;

    // The goal of this SQL statement is to select the oldest notes until the required
    // value has been reached, and then fetch the witnesses at the desired height for the
    // selected notes. This is achieved in several steps:
    //
    // 1) Use a window function to create a view of all notes, ordered from oldest to
    //    newest, with an additional column containing a running sum:
    //    - Unspent notes accumulate the values of all unspent notes in that note's
    //      account, up to itself.
    //    - Spent notes accumulate the values of all notes in the transaction they were
    //      spent in, up to itself.
    //
    // 2) Select all unspent notes in the desired account, along with their running sum.
    //
    // 3) Select all notes for which the running sum was less than the required value, as
    //    well as a single note for which the sum was greater than or equal to the
    //    required value, bringing the sum of all selected notes across the threshold.
    //
    // 4) Match the selected notes against the witnesses at the desired height.
    let mut stmt_select_notes = data.prepare(
        "WITH selected AS (
            WITH eligible AS (
                SELECT id_note, diversifier, value, rcm,
                    SUM(value) OVER
                        (PARTITION BY account, spent ORDER BY id_note) AS so_far
                FROM received_notes
                WHERE account = ? AND spent IS NULL
            )
            SELECT * FROM eligible WHERE so_far < ?
            UNION
            SELECT * FROM (SELECT * FROM eligible WHERE so_far >= ? LIMIT 1)
        ), witnesses AS (
            SELECT note, witness FROM sapling_witnesses
            WHERE block = ?
        )
        SELECT selected.diversifier, selected.value, selected.rcm, witnesses.witness
        FROM selected
        INNER JOIN witnesses ON selected.id_note = witnesses.note",
    )?;

    // Select notes
    let notes = stmt_select_notes.query_and_then::<_, Error, _, _>(
        &[
            account as i64,
            value.0,
            value.0,
            (height - ANCHOR_OFFSET) as i64,
        ],
        |row| {
            let mut diversifier = Diversifier([0; 11]);
            let d: Vec<_> = row.get(0);
            diversifier.0.copy_from_slice(&d);

            let note_value: i64 = row.get(1);

            let d: Vec<_> = row.get(2);
            let rcm = {
                let mut tmp = FsRepr::default();
                tmp.read_le(&d[..])?;
                Fs::from_repr(tmp)?
            };

            let from = extfvk
                .fvk
                .vk
                .into_payment_address(diversifier, &JUBJUB)
                .unwrap();
            let note = from.create_note(note_value as u64, rcm, &JUBJUB).unwrap();

            let d: Vec<_> = row.get(3);
            let witness = IncrementalWitness::read(&d[..])?;

            Ok(SelectedNoteRow {
                diversifier,
                note,
                witness,
            })
        },
    )?;

    // Create the transaction
    let mut builder = Builder::new(height);
    for selected in notes {
        let selected = selected?;
        builder.add_sapling_spend(
            extsk.clone(),
            selected.diversifier,
            selected.note,
            selected.witness,
        )?;
    }
    builder.add_sapling_output(ovk, to.clone(), value, memo.clone())?;
    let (tx, tx_metadata) = builder.build(consensus_branch_id, prover)?;
    // We only called add_sapling_output() once.
    let output_index = tx_metadata.output_index(0).unwrap() as i64;

    // Save the transaction in the database.
    let mut raw_tx = vec![];
    tx.write(&mut raw_tx)?;
    let mut stmt_insert_tx = data.prepare(
        "INSERT INTO transactions (txid, raw)
        VALUES (?, ?)",
    )?;
    stmt_insert_tx.execute(&[&tx.txid().0[..], &raw_tx[..]])?;
    let id_tx = data.last_insert_rowid();

    // Save the sent note in the database.
    let to_str = encode_payment_address(HRP_SAPLING_PAYMENT_ADDRESS_TEST, to);
    if memo.is_some() {
        let mut stmt_insert_sent_note = data.prepare(
            "INSERT INTO sent_notes (tx, output_index, from_account, address, value, memo)
            VALUES (?, ?, ?, ?, ?, ?)",
        )?;
        stmt_insert_sent_note.execute(&[
            id_tx.to_sql()?,
            output_index.to_sql()?,
            account.to_sql()?,
            to_str.to_sql()?,
            value.0.to_sql()?,
            memo.unwrap().as_bytes().to_sql()?,
        ])?;
    } else {
        let mut stmt_insert_sent_note = data.prepare(
            "INSERT INTO sent_notes (tx, output_index, from_account, address, value)
            VALUES (?, ?, ?, ?, ?)",
        )?;
        stmt_insert_sent_note.execute(&[
            id_tx.to_sql()?,
            output_index.to_sql()?,
            account.to_sql()?,
            to_str.to_sql()?,
            value.0.to_sql()?,
        ])?;
    }

    // Return the row number of the transaction, so the caller can fetch it for sending.
    Ok(id_tx)
}
