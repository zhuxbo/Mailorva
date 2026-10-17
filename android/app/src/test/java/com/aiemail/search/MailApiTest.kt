package com.aiemail.search

import kotlinx.coroutines.test.runTest
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import org.junit.Assert.*
import org.junit.Test

class MailApiTest {
    @Test fun queryEncodesChineseAndSpecialCharactersAndAddsBearer() = runTest {
        MockWebServer().use { server ->
            server.enqueue(MockResponse().setBody("""{"items":[],"total":0,"limit":30,"offset":30,"sync":[]}"""))
            val api = HttpMailApi(Connection(server.url("/mail/").toString(), "readonly"))
            val result = api.messages("证书 & a+b@example.com", "工作", 30)
            assertEquals(0, result.total)
            val request = server.takeRequest()
            assertEquals("Bearer readonly", request.getHeader("Authorization"))
            assertEquals("/mail/api/messages", request.requestUrl!!.encodedPath)
            assertEquals("证书 & a+b@example.com", request.requestUrl!!.queryParameter("q"))
            assertEquals("工作", request.requestUrl!!.queryParameter("account_id"))
            assertEquals("30", request.requestUrl!!.queryParameter("offset"))
        }
    }
    @Test fun parsesPlainTextDetailAndIgnoresUnknownFields() = runTest {
        MockWebServer().use { server ->
            server.enqueue(MockResponse().setBody("""{"id":"id1","account_id":"work","account_email":"me@example.com","folder":"INBOX","subject":"证书","from_address":"a@example.com","to_addresses":["me@example.com"],"received_at":"2026-09-26T00:00:00Z","body_preview":"预览","flags":[],"body_text":"正文 <script>原样文字</script>","cc_addresses":[],"message_id":null,"references":[],"future":true}"""))
            val message = HttpMailApi(Connection(server.url("/").toString(), "token")).message("id1")
            assertEquals("正文 <script>原样文字</script>", message.bodyText)
            assertEquals("a@example.com", message.fromAddress)
        }
    }
    @Test fun mapsUnauthorizedAndMalformedResponsesWithoutLeakingRawBody() = runTest {
        MockWebServer().use { server ->
            val api = HttpMailApi(Connection(server.url("/").toString(), "token"))
            server.enqueue(MockResponse().setResponseCode(401).setBody("secret proxy diagnostic"))
            try { api.accounts(); fail("expected error") } catch (error: ApiException) { assertTrue(error.message!!.contains("令牌")); assertFalse(error.message!!.contains("secret")) }
            server.enqueue(MockResponse().setBody("secret broken json"))
            try { api.accounts(); fail("expected error") } catch (error: ApiException) { assertFalse(error.message!!.contains("secret")) }
        }
    }
    @Test fun redirectsAreNotFollowedWithCredentials() = runTest {
        MockWebServer().use { server ->
            server.enqueue(MockResponse().setResponseCode(302).setHeader("Location", server.url("/unexpected")))
            try { HttpMailApi(Connection(server.url("/").toString(), "token")).accounts(); fail("expected error") } catch (_: ApiException) { }
            assertEquals(1, server.requestCount)
        }
    }
    @Test fun parsesWriteCapabilityAndReplyUsesServerPreview() = runTest {
        MockWebServer().use { server ->
            val api = HttpMailApi(Connection(server.url("/mail/").toString(), "writer"))
            server.enqueue(MockResponse().setBody("""{"accounts":[],"can_write":true}"""))
            assertTrue(api.accounts().canWrite)
            server.takeRequest()
            server.enqueue(MockResponse().setBody("""{"operation_id":"operation","status":"prepared","message_id":"source","account_id":"work","account_email":"me@example.com","to":["reply@example.com"],"subject":"Re: 证书","body_text":"谢谢\n已收到"}"""))
            val preview = api.prepareReply("source", "operation", "谢谢\n已收到")
            assertEquals(listOf("reply@example.com"), preview.to)
            assertEquals("谢谢\n已收到", preview.bodyText)
            val prepare = server.takeRequest()
            assertEquals("POST", prepare.method)
            assertEquals("/mail/api/messages/source/reply/prepare", prepare.path)
            assertEquals("Bearer writer", prepare.getHeader("Authorization"))
            val requestBody = kotlinx.serialization.json.Json.parseToJsonElement(prepare.body.readUtf8()).toString()
            assertTrue(requestBody.contains("operation_id"))
            assertTrue(requestBody.contains("\"include_original\":true"))
            assertFalse(requestBody.contains("reply@example.com"))
            server.enqueue(MockResponse().setBody("""{"operation_id":"operation","status":"sent","message_id":"sent-id"}"""))
            assertEquals("sent", api.sendReply("operation").status)
            val send = server.takeRequest()
            assertEquals("POST", send.method)
            assertEquals("/mail/api/send/operation/send", send.path)
            server.enqueue(MockResponse().setBody("""{"operation_id":"operation","status":"unknown","message_id":null}"""))
            assertEquals("unknown", api.replyStatus("operation").status)
            val status = server.takeRequest()
            assertEquals("GET", status.method)
            assertEquals("/mail/api/send/operation", status.path)
        }
    }

}
