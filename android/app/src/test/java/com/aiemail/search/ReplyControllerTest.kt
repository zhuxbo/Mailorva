package com.aiemail.search

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.*
import org.junit.Test

@OptIn(kotlinx.coroutines.ExperimentalCoroutinesApi::class)
class ReplyControllerTest {
    private class MemoryPending : PendingReplyStore {
        var value: PendingReply? = null
        var failSave = false
        override fun loadPending() = value
        override fun savePending(reply: PendingReply) { check(!failSave); value = reply }
        override fun clearPending() { value = null }
    }
    private class FakeApi : ReplyApi {
        var sends = 0
        var prepares = 0
        var includedOriginal: Boolean? = null
        var queries = 0
        var failSend = false
        var failPrepare = false
        var status = "prepared"
        var sendGate: CompletableDeferred<Unit>? = null
        override suspend fun prepareReply(messageId: String, operationId: String, body: String, includeOriginal: Boolean): ReplyPreview {
            prepares++
            includedOriginal = includeOriginal
            if (failPrepare) throw ApiException("network")
            return ReplyPreview(operationId, "prepared", "source", "work", "me@example.com", listOf("reply@example.com"), "Re: Test", body)
        }
        override suspend fun sendReply(operationId: String): SendStatus {
            sends++
            sendGate?.await()
            if (failSend) throw ApiException("network")
            return SendStatus(operationId, "sent", "sent-message")
        }
        override suspend fun replyStatus(operationId: String): SendStatus { queries++; return SendStatus(operationId, status) }
    }
    @Test fun quoteDefaultsOnAndCanBeDisabled() = runTest {
        val api = FakeApi()
        val controller = ReplyController(api, this, "https://mail/", MemoryPending())
        controller.start(MailMessage("source"), true)
        controller.prepare("hello")
        runCurrent()
        assertEquals(true, api.includedOriginal)
        controller.acknowledgeAndClear()
        controller.start(MailMessage("source"), true)
        controller.prepare("hello", false)
        runCurrent()
        assertEquals(false, api.includedOriginal)
        assertEquals("hello", controller.state.value.preview!!.bodyText)
    }
    @Test fun readonlyCannotPrepare() = runTest {
        val api = FakeApi()
        val controller = ReplyController(api, this, "https://mail/", MemoryPending())
        controller.start(MailMessage("source"), false)
        controller.prepare("hello")
        runCurrent()
        assertEquals(0, api.prepares)
        assertNotNull(controller.state.value.error)
    }
    @Test fun previewRequiresExplicitSendAndDoubleTapSendsOnce() = runTest {
        val api = FakeApi().apply { sendGate = CompletableDeferred() }
        val store = MemoryPending()
        val controller = ReplyController(api, this, "https://mail/", store)
        controller.start(MailMessage("source"), true)
        controller.prepare("hello")
        runCurrent()
        assertEquals(0, api.sends)
        assertEquals(listOf("reply@example.com"), controller.state.value.preview!!.to)
        controller.send(true)
        controller.send(true)
        runCurrent()
        assertEquals(1, api.sends)
        assertNotNull(store.value)
        api.sendGate!!.complete(Unit)
        runCurrent()
        assertNull(store.value)
        assertEquals("sent", controller.state.value.status)
    }
    @Test fun persistenceFailurePreventsSend() = runTest {
        val api = FakeApi()
        val store = MemoryPending()
        val controller = ReplyController(api, this, "https://mail/", store)
        controller.start(MailMessage("source"), true)
        controller.prepare("hello")
        runCurrent()
        store.failSave = true
        controller.send(true)
        runCurrent()
        assertEquals(0, api.sends)
    }
    @Test fun unknownSendOnlyQueriesSameOperationAfterRestart() = runTest {
        val api = FakeApi().apply { failSend = true; status = "unknown" }
        val store = MemoryPending()
        val controller = ReplyController(api, this, "https://mail/", store)
        controller.start(MailMessage("source"), true)
        controller.prepare("hello")
        runCurrent()
        controller.send(true)
        runCurrent()
        val id = store.value!!.operationId
        controller.send(true)
        controller.prepare("again")
        runCurrent()
        val restored = ReplyController(api, this, "https://mail/", store)
        restored.restore()
        runCurrent()
        assertEquals(1, api.sends)
        assertEquals(1, api.prepares)
        assertEquals(1, api.queries)
        assertEquals(id, store.value!!.operationId)
        assertEquals("unknown", restored.state.value.status)
    }
    @Test fun changedServerNeverQueriesPendingOnNewServer() = runTest {
        val api = FakeApi()
        val store = MemoryPending().apply { value = PendingReply("https://old/", "operation") }
        val controller = ReplyController(api, this, "https://new/", store)
        controller.restore()
        runCurrent()
        controller.checkStatus()
        runCurrent()
        assertEquals(0, api.queries)
        assertTrue(controller.state.value.error!!.contains("https://old/"))
        assertNotNull(store.value)
    }
    @Test fun uncertainPrepareKeepsOperationAndNeverAutomaticallyPreparesAgain() = runTest {
        val api = FakeApi().apply { failPrepare = true }
        val store = MemoryPending()
        val controller = ReplyController(api, this, "https://mail/", store)
        controller.start(MailMessage("source"), true)
        controller.prepare("hello")
        runCurrent()
        val id = store.value!!.operationId
        controller.prepare("again")
        controller.send(true)
        controller.checkStatus()
        runCurrent()
        assertEquals(1, api.prepares)
        assertEquals(0, api.sends)
        assertEquals(id, store.value!!.operationId)
        assertNull(controller.state.value.preview)
    }
    @Test fun restoredPreparedReplyRequiresNewExplicitConfirmation() = runTest {
        val api = FakeApi()
        val store = MemoryPending().apply {
            value = PendingReply("https://mail/", "operation", "work", ReplyPreview("operation", "prepared", "source", "work", "me@example.com", listOf("to@example.com"), "Re: Test", "hello"))
        }
        val controller = ReplyController(api, this, "https://mail/", store)
        controller.restore()
        runCurrent()
        assertEquals("prepared", controller.state.value.status)
        assertEquals(0, api.sends)
        controller.send(false)
        runCurrent()
        assertEquals(0, api.sends)
        controller.send(true)
        runCurrent()
        assertEquals(1, api.sends)
        assertNull(store.value)
    }
    @Test fun failedInitialPersistenceDoesNotEvenPrepare() = runTest {
        val api = FakeApi()
        val store = MemoryPending().apply { failSave = true }
        val controller = ReplyController(api, this, "https://mail/", store)
        controller.start(MailMessage("source"), true)
        controller.prepare("hello")
        runCurrent()
        assertEquals(0, api.prepares)
        assertEquals(0, api.sends)
        assertNull(store.value)
    }
    @Test fun pendingBlocksNewReplyUntilExplicitAcknowledgement() = runTest {
        val api = FakeApi().apply { status = "unknown" }
        val store = MemoryPending().apply { value = PendingReply("https://mail/", "operation") }
        val controller = ReplyController(api, this, "https://mail/", store)
        controller.restore()
        runCurrent()
        controller.start(MailMessage("new-source"), true)
        controller.prepare("hello")
        runCurrent()
        assertEquals(0, api.prepares)
        assertEquals("operation", store.value!!.operationId)
        controller.acknowledgeAndClear()
        controller.start(MailMessage("new-source"), true)
        controller.prepare("hello")
        runCurrent()
        assertEquals(1, api.prepares)
    }

}
