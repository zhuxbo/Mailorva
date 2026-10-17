package com.aiemail.search

import java.io.IOException
import java.util.concurrent.TimeUnit
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.serialization.json.Json
import okhttp3.Call
import okhttp3.Callback
import okhttp3.HttpUrl
import okhttp3.HttpUrl.Companion.toHttpUrl
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.RequestBody.Companion.toRequestBody
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.put

class HttpMailApi(private val connection: Connection) : MailApi, ReplyApi {
    private val client = OkHttpClient.Builder()
        .connectTimeout(15, TimeUnit.SECONDS).readTimeout(30, TimeUnit.SECONDS)
        .callTimeout(45, TimeUnit.SECONDS)
        .followRedirects(false).followSslRedirects(false).retryOnConnectionFailure(false).build()
    private val json = Json { ignoreUnknownKeys = true }
    private fun endpoint(path: String) = (connection.url + path).toHttpUrl()
    override suspend fun accounts(): AccountResponse = get(endpoint("api/accounts"))
    override suspend fun messages(query: String, account: String?, offset: Int): MessagePage = get(
        endpoint("api/messages").newBuilder().addQueryParameter("q", query)
            .addQueryParameter("limit", "30").addQueryParameter("offset", offset.toString())
            .apply { account?.let { addQueryParameter("account_id", it) } }.build()
    )
    override suspend fun message(id: String): MailMessage = get(endpoint("api/messages").newBuilder().addPathSegment(id).build())

    override suspend fun prepareReply(messageId: String, operationId: String, body: String, includeOriginal: Boolean): ReplyPreview = request(
        endpoint("api/messages").newBuilder().addPathSegment(messageId).addPathSegment("reply").addPathSegment("prepare").build(),
        buildJsonObject { put("operation_id", operationId); put("body_text", body); put("include_original", includeOriginal) }.toString(),
    )
    override suspend fun sendReply(operationId: String): SendStatus = request(endpoint("api/send").newBuilder().addPathSegment(operationId).addPathSegment("send").build(), "{}")
    override suspend fun replyStatus(operationId: String): SendStatus = get(endpoint("api/send").newBuilder().addPathSegment(operationId).build())
    private suspend inline fun <reified T> get(url: HttpUrl): T = request(url)
    private suspend inline fun <reified T> request(url: HttpUrl, body: String? = null): T = suspendCancellableCoroutine { continuation ->
        val builder = Request.Builder().url(url).header("Authorization", "Bearer ${connection.token}").header("Accept", "application/json")
        body?.let { builder.post(it.toRequestBody("application/json; charset=utf-8".toMediaType())) }
        val call = client.newCall(builder.build())
        continuation.invokeOnCancellation { call.cancel() }
        call.enqueue(object : Callback {
            override fun onFailure(call: Call, e: IOException) {
                if (continuation.isActive) continuation.resumeWithException(ApiException("无法连接服务器，请检查网络、服务器地址和 HTTPS 证书。"))
            }
            override fun onResponse(call: Call, response: Response) {
                response.use {
                    try {
                        if (!response.isSuccessful) throw ApiException(when (response.code) {
                            401, 403 -> "访问令牌无效或权限不足；回复及状态核对需要可写令牌。"
                            404 -> "邮件或操作不存在，请核对服务器与记录。"
                            400 -> "请求参数无效，请检查输入。"
                            503 -> "服务器暂时不可用，请稍后重试。"
                            in 300..399 -> "服务器返回重定向，请在设置中填写最终 HTTPS 地址。"
                            else -> "服务器请求失败（${response.code}），请稍后重试。"
                        })
                        val body = response.body?.string() ?: throw ApiException("服务器返回了空响应。")
                        val result = try { json.decodeFromString<T>(body) } catch (_: Exception) { throw ApiException("服务器响应格式不正确，请检查服务版本和地址。") }
                        if (continuation.isActive) continuation.resume(result)
                    } catch (error: Exception) {
                        if (continuation.isActive) continuation.resumeWithException(if (error is ApiException) error else ApiException("读取服务器响应失败，请重试。"))
                    }
                }
            }
        })
    }
}
