package cash.z.wallet.sdk.transaction

import android.content.Context
import androidx.room.Room
import androidx.room.RoomDatabase
import cash.z.wallet.sdk.db.PendingTransactionDao
import cash.z.wallet.sdk.db.PendingTransactionDb
import cash.z.wallet.sdk.entity.*
import cash.z.wallet.sdk.ext.twig
import cash.z.wallet.sdk.service.LightWalletService
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flow
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlin.math.max

/**
 * Facilitates persistent attempts to ensure a transaction occurs.
 */
// TODO: consider having the manager register the fail listeners rather than having that responsibility spread elsewhere (synchronizer and the broom)
class PersistentTransactionManager(
    db: PendingTransactionDb,
    private val encoder: TransactionEncoder,
    private val service: LightWalletService
) : OutboundTransactionManager {

    private val daoMutex = Mutex()

    /**
     * Internal reference to the dao that is only accessed after locking the [daoMutex] in order
     * to enforce DB access in both a threadsafe and coroutinesafe way.
     */
    private val _dao: PendingTransactionDao = db.pendingTransactionDao()

    /**
     * Constructor that creates the database and then executes a callback on it.
     */
    constructor(
        appContext: Context,
        encoder: TransactionEncoder,
        service: LightWalletService,
        dataDbName: String = "PendingTransactions.db"
    ) : this(
        Room.databaseBuilder(
            appContext,
            PendingTransactionDb::class.java,
            dataDbName
        ).setJournalMode(RoomDatabase.JournalMode.TRUNCATE).build(),
        encoder,
        service
    )

    /**
     * Initialize a [PendingTransaction] and then insert it in the database for monitoring and
     * follow-up.
     */
    override fun initSpend(
        zatoshiValue: Long,
        toAddress: String,
        memo: String,
        fromAccountIndex: Int
    ): Flow<PendingTransaction> = flow {
        twig("constructing a placeholder transaction")
        var tx = PendingTransactionEntity(
            value = zatoshiValue,
            toAddress = toAddress,
            memo = memo.toByteArray(),
            accountIndex = fromAccountIndex
        )
        try {
            twig("creating tx in DB: $tx")
            pendingTransactionDao {
                val insertedTx = findById(create(tx))
                twig("pending transaction created with id: ${insertedTx?.id}")
                tx = tx.copy(id = insertedTx!!.id)
            }.also {
                twig("successfully created TX in DB")
            }
        } catch (t: Throwable) {
            twig("Unknown error while attempting to create pending transaction: ${t.message} caused by: ${t.cause}")
        }

        emit(tx)
    }

    suspend fun manageMined(pendingTx: PendingTransactionEntity, matchingMinedTx: TransactionEntity) {
        twig("a pending transaction has been mined!")
        safeUpdate(pendingTx.copy(minedHeight = matchingMinedTx.minedHeight!!))
    }

    /**
     * Remove a transaction and pretend it never existed.
     */
    suspend fun abortTransaction(existingTransaction: PendingTransaction) {
        pendingTransactionDao {
            delete(existingTransaction as PendingTransactionEntity)
        }
    }

    override fun encode(
        spendingKey: String,
        pendingTx: PendingTransaction
    ): Flow<PendingTransaction> = flow {
        twig("managing the creation of a transaction")
        //var tx = transaction.copy(expiryHeight = if (currentHeight == -1) -1 else currentHeight + EXPIRY_OFFSET)
        var tx = pendingTx as PendingTransactionEntity
        try {
            twig("beginning to encode transaction with : $encoder")
            val encodedTx = encoder.createTransaction(
                spendingKey,
                tx.value,
                tx.toAddress,
                tx.memo,
                tx.accountIndex
            )
            twig("successfully encoded transaction for ${tx.memo}!!")
            tx = tx.copy(raw = encodedTx.raw, rawTransactionId = encodedTx.txId)
        } catch (t: Throwable) {
            val message = "failed to encode transaction due to : ${t.message} caused by: ${t.cause}"
            twig(message)
            message
            tx = tx.copy(errorMessage = message, errorCode = 2000) //TODO: find a place for these error codes
        } finally {
            tx = tx.copy(encodeAttempts = max(1, tx.encodeAttempts + 1))
        }
        safeUpdate(tx)

        emit(tx)
    }

    override fun submit(pendingTx: PendingTransaction): Flow<PendingTransaction> = flow {
        var tx1 = pendingTransactionDao { findById(pendingTx.id) }
        if(tx1 == null) twig("unable to find transaction for id: ${pendingTx.id}")
        var tx = tx1!!
        try {
            // do nothing when cancelled
            if (!tx.isCancelled()) {
                twig("submitting transaction to lightwalletd - memo: ${tx.memo} amount: ${tx.value}")
                val response = service.submitTransaction(tx.raw!!)
                val error = response.errorCode < 0
                twig("${if (error) "FAILURE! " else "SUCCESS!"} submit transaction completed with response: ${response.errorCode}: ${response.errorMessage}")
                tx = tx.copy(
                    errorMessage = if (error) response.errorMessage else null,
                    errorCode = response.errorCode,
                    submitAttempts = max(1, tx.submitAttempts + 1)
                )
                safeUpdate(tx)
            } else {
                twig("Warning: ignoring cancelled transaction with id ${tx.id}")
            }
        } catch (t: Throwable) {
            // a non-server error has occurred
            val message =
                "Unknown error while submitting transaction: ${t.message} caused by: ${t.cause}"
            twig(message)
            tx = tx.copy(errorMessage = t.message, errorCode = 3000, submitAttempts = max(1, tx.submitAttempts + 1)) //TODO: find a place for these error codes
            safeUpdate(tx)
        }

        emit(tx)
    }

    override suspend fun cancel(pendingTx: PendingTransaction): Boolean {
        return pendingTransactionDao {
            val tx = findById(pendingTx.id)
            if (tx?.isSubmitted() == true) {
                false
            } else {
                cancel(pendingTx.id)
                true
            }
        }
    }

    override suspend fun getAll(): List<PendingTransaction> = pendingTransactionDao { getAll() }

    /**
     * Updating the pending transaction is often done at the end of a function but still should
     * happen within a try/catch block, surrounded by logging. So this helps with that.
     */
    private suspend fun safeUpdate(tx: PendingTransactionEntity): PendingTransaction {
        return try {
            twig("updating tx into DB: $tx")
            pendingTransactionDao { update(tx) }
            twig("successfully updated TX into DB")
            tx
        } catch (t: Throwable) {
            twig("Unknown error while attempting to update pending transaction: ${t.message} caused by: ${t.cause}")
            tx
        }
    }

    private suspend fun <T> pendingTransactionDao(block: suspend PendingTransactionDao.() -> T): T {
        return daoMutex.withLock {
            _dao.block()
        }
    }
}

