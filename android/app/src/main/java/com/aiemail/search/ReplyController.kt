package com.aiemail.search

import java.util.UUID
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable

@Serializable data class ReplyPreview(
    @SerialName("operation_id") val operationId: String,
    val status: String,
    @SerialName("message_id") val messageId: String? = null,
    @SerialName("account_id") val accountId: String,
    @SerialName("account_email") val accountEmail: String,
    val to: List<String>,
    val subject: String,
    @SerialName("body_text") val bodyText: String,
)
@Serializable data class SendStatus(
    @SerialName("operation_id") val operationId: String,
    val status: String,
    @SerialName("message_id") val messageId: String? = null,
)
@Serializable data class PendingReply(val serverUrl: String, val operationId: String, val accountId: String? = null, val preview: ReplyPreview? = null)
interface PendingReplyStore {
    fun loadPending(): PendingReply?
    fun savePending(reply: PendingReply)
    fun clearPending()
}
interface ReplyApi {
    suspend fun prepareReply(messageId: String, operationId: String, body: String, includeOriginal: Boolean = true): ReplyPreview
    suspend fun sendReply(operationId: String): SendStatus
    suspend fun replyStatus(operationId: String): SendStatus
}
data class ReplyState(
    val visible: Boolean = false, val messageId: String? = null,
    val pending: PendingReply? = null, val preview: ReplyPreview? = null,
    val busy: Boolean = false, val status: String? = null, val error: String? = null,
)
class ReplyController(private val api: ReplyApi, private val scope: CoroutineScope, private val serverUrl: String, private val store: PendingReplyStore) {
    private val mutableState = MutableStateFlow(ReplyState())
    val state = mutableState.asStateFlow()
    private var storageUnreadable = false

    fun restore() {
        try {
            val pending = store.loadPending() ?: return
            mutableState.value = ReplyState(visible = true, pending = pending, preview = pending.preview, status = "unknown")
            checkStatus()
        } catch (_: Exception) {
            storageUnreadable = true
            mutableState.value = ReplyState(visible = true, error = "无法读取待核对的回复记录；请先在邮箱核对发送情况，避免重复发送。")
        }
    }
    fun start(message: MailMessage, canWrite: Boolean) {
        if (state.value.busy) return
        if (state.value.pending != null || storageUnreadable) { show(); return }
        mutableState.value = if (canWrite) ReplyState(visible = true, messageId = message.id)
        else ReplyState(visible = true, error = "当前访问令牌仅可搜索阅读；请在设置中切换可写令牌后回复。")
    }
    fun show() { mutableState.update { it.copy(visible = true) } }
    fun hide() { mutableState.update { it.copy(visible = false) } }
    fun prepare(body: String, includeOriginal: Boolean = true) {
        val current = state.value
        val messageId = current.messageId ?: return
        if (current.busy || current.pending != null || storageUnreadable) return
        if (body.isBlank()) { mutableState.update { it.copy(error = "请输入回复正文。") }; return }
        val pending = PendingReply(serverUrl, UUID.randomUUID().toString())
        try { store.savePending(pending) }
        catch (_: Exception) { mutableState.update { it.copy(error = "无法安全保存回复记录，尚未准备或发送，请重试。") }; return }
        mutableState.update { it.copy(busy = true, pending = pending, error = null) }
        scope.launch {
            try {
                val preview = api.prepareReply(messageId, pending.operationId, body, includeOriginal)
                check(preview.operationId == pending.operationId)
                val updated = pending.copy(accountId = preview.accountId, preview = preview)
                store.savePending(updated)
                mutableState.update { it.copy(pending = updated, preview = preview, status = preview.status, busy = false) }
            } catch (error: CancellationException) { throw error }
            catch (error: Exception) { uncertain(error) }
        }
    }
    fun send(canWrite: Boolean) {
        val current = state.value
        val pending = current.pending ?: return
        if (current.busy || current.status != "prepared" || current.preview == null || pending.serverUrl != serverUrl) return
        if (!canWrite) { mutableState.update { it.copy(error = "请切换可写访问令牌后发送。") }; return }
        // 先同步持久化，再允许网络发送；进程退出也不会失去操作编号。
        try { store.savePending(pending) }
        catch (_: Exception) { mutableState.update { it.copy(error = "无法安全保存回复记录，尚未发送，请重试。") }; return }
        mutableState.update { it.copy(busy = true, status = "sending", error = null) }
        scope.launch {
            try { applyStatus(api.sendReply(pending.operationId), pending.operationId) }
            catch (error: CancellationException) { throw error }
            catch (error: Exception) { uncertain(error) }
        }
    }
    fun checkStatus() {
        val current = state.value
        val pending = current.pending ?: return
        if (current.busy) return
        if (pending.serverUrl != serverUrl) {
            mutableState.update { it.copy(error = "待核对的回复属于 ${pending.serverUrl}，请切回该服务器并使用可写令牌核对；当前服务器不会接收此操作。") }
            return
        }
        mutableState.update { it.copy(busy = true, error = null) }
        scope.launch {
            try { applyStatus(api.replyStatus(pending.operationId), pending.operationId) }
            catch (error: CancellationException) { throw error }
            catch (error: Exception) { uncertain(error) }
        }
    }
    private fun applyStatus(result: SendStatus, operationId: String) {
        check(result.operationId == operationId)
        if (result.status == "sent") store.clearPending()
        mutableState.update { it.copy(busy = false, status = result.status, pending = if (result.status == "sent") null else it.pending, error = null) }
    }
    private fun uncertain(error: Exception) {
        val reason = if (error is ApiException) error.message else "请求或本机保存失败。"
        mutableState.update { it.copy(busy = false, status = "unknown", error = "$reason 状态尚未确认，请查询原操作或在邮箱核对；不会自动重发。") }
    }
    fun acknowledgeAndClear() {
        if (state.value.busy) return
        try { store.clearPending(); storageUnreadable = false; mutableState.value = ReplyState() }
        catch (_: Exception) { mutableState.update { it.copy(error = "无法清除回复记录，请重试。") } }
    }
}
