package com.aiemail.search

import android.os.Bundle
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import java.time.OffsetDateTime
import java.time.format.DateTimeFormatter

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.setFlags(WindowManager.LayoutParams.FLAG_SECURE, WindowManager.LayoutParams.FLAG_SECURE)
        enableEdgeToEdge()
        setContent {
            val colors = if (isSystemInDarkTheme()) darkColorScheme(primary = Color(0xFFABC8C0)) else lightColorScheme(primary = Color(0xFF285B50), background = Color(0xFFF8FAF8))
            MaterialTheme(colorScheme = colors) { MailApp() }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable private fun MailApp(model: MailViewModel = viewModel()) {
    val controller by model.controller.collectAsStateWithLifecycle()
    val replyController by model.replyController.collectAsStateWithLifecycle()
    var showSettings by rememberSaveable { mutableStateOf(false) }
    val settings by model.settings.collectAsStateWithLifecycle()
    val error by model.settingsError.collectAsStateWithLifecycle()
    val active = controller
    if (showSettings || active == null) {
        SettingsScreen(settings, error, active != null, onSave = { if (model.save(it)) showSettings = false }, onBack = { showSettings = false }, onClear = model::disconnect)
        return
    }
    val state by active.state.collectAsStateWithLifecycle()
    val reply = replyController
    val replyState = reply?.state?.collectAsStateWithLifecycle()?.value
    if (reply != null && replyState?.visible == true) ReplyDialog(replyState, reply, state.canWrite)
    BackHandler(state.selectedId != null) { active.closeMessage() }
    Scaffold(topBar = {
        TopAppBar(title = { Text(if (state.selectedId == null) "Mailorva" else "邮件详情") },
            navigationIcon = { if (state.selectedId != null) TextButton(onClick = active::closeMessage) { Text("返回") } },
            actions = {
                if (replyState?.pending != null || replyState?.error != null) TextButton(onClick = { reply?.show() }) { Text("回复核对") }
                TextButton(onClick = { showSettings = true }) { Text("设置") }
            })
    }) { padding ->
        if (state.selectedId != null) DetailScreen(state, active, reply, Modifier.padding(padding))
        else SearchScreen(state, active, Modifier.padding(padding))
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable private fun SettingsScreen(saved: SavedSettings, error: String?, canBack: Boolean, onSave: (SavedSettings) -> Unit, onBack: () -> Unit, onClear: () -> Unit) {
    // 令牌仅保留在内存中，不写入 Compose 的可保存状态。
    var url by remember(saved.url) { mutableStateOf(saved.url) }
    var token by remember(saved.token) { mutableStateOf(saved.token) }
    var allowHttp by remember(saved.allowEmulatorHttp) { mutableStateOf(saved.allowEmulatorHttp) }
    BackHandler(canBack, onBack)
    Scaffold(topBar = { TopAppBar(title = { Text("连接服务器") }, navigationIcon = { if (canBack) TextButton(onClick = onBack) { Text("返回") } }) }) { padding ->
        LazyColumn(Modifier.padding(padding).imePadding().fillMaxSize(), contentPadding = PaddingValues(24.dp), verticalArrangement = Arrangement.spacedBy(20.dp)) {
            item { Text("在手机上查找邮件", style = MaterialTheme.typography.headlineSmall) }
            item { Text("连接你的邮件聚合服务器。read 令牌可搜索阅读，write 令牌还可回复邮件。", color = MaterialTheme.colorScheme.onSurfaceVariant) }
            item { OutlinedTextField(url, { url = it }, label = { Text("服务器 URL") }, placeholder = { Text("https://mail.example.com") }, singleLine = true, keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri), modifier = Modifier.fillMaxWidth()) }
            item { OutlinedTextField(token, { token = it }, label = { Text("访问令牌（Bearer）") }, singleLine = true, visualTransformation = PasswordVisualTransformation(), keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password), modifier = Modifier.fillMaxWidth()) }
            if (BuildConfig.DEBUG) item { Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) { Text("允许模拟器 HTTP\n仅限 10.0.2.2", Modifier.weight(1f)); Switch(allowHttp, { allowHttp = it }) } }
            if (error != null) item { Text(error, color = MaterialTheme.colorScheme.error) }
            item { Button(onClick = { onSave(SavedSettings(url, token, allowHttp)) }, modifier = Modifier.fillMaxWidth()) { Text("保存并连接") } }
            item { Text("连接信息和待核对回复在本机加密保存，不参与系统备份。邮件只在打开应用时查询，不发送通知。清除连接设置会保留待核对回复记录。", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant) }
            if (canBack) item { TextButton(onClick = onClear) { Text("清除连接设置") } }
        }
    }
}

@Composable private fun SearchScreen(state: SearchState, controller: SearchController, modifier: Modifier) {
    var query by rememberSaveable { mutableStateOf(state.query) }
    var accountsExpanded by remember { mutableStateOf(false) }
    LazyColumn(modifier.fillMaxSize(), contentPadding = PaddingValues(20.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        item { Text("查找保留范围内的邮件", style = MaterialTheme.typography.titleLarge) }
        item { OutlinedTextField(query, { query = it; controller.invalidateSearch() }, label = { Text("标题、正文或邮箱关键词") }, supportingText = { Text("多个词用空格分隔；留空查看最近邮件") }, keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search), keyboardActions = KeyboardActions(onSearch = { controller.search(query, state.accountId) }), singleLine = true, modifier = Modifier.fillMaxWidth()) }
        item {
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                Box(Modifier.weight(1f)) {
                    OutlinedButton(onClick = { accountsExpanded = true }, modifier = Modifier.fillMaxWidth()) { Text(state.accounts.find { it.id == state.accountId }?.let { it.displayName.ifBlank { it.email } } ?: "全部账户", maxLines = 1, overflow = TextOverflow.Ellipsis) }
                    DropdownMenu(accountsExpanded, { accountsExpanded = false }) {
                        DropdownMenuItem(text = { Text("全部账户") }, onClick = { accountsExpanded = false; controller.search(query, null) })
                        state.accounts.forEach { account -> DropdownMenuItem(text = { Text(account.displayName.ifBlank { account.email } + "\n" + account.email) }, onClick = { accountsExpanded = false; controller.search(query, account.id) }) }
                    }
                }
                Button(onClick = { controller.search(query, state.accountId) }) { Text("搜索") }
            }
        }
        if (state.accountsError != null) item { ErrorMessage(state.accountsError, controller::refreshAccounts) }
        val pending = state.sync.count { it.status == "pending" }
        val failures = state.sync.count { it.status == "error" }
        if (pending > 0 || failures > 0) item { Text("同步状态：${pending} 个账户待同步，${failures} 个账户同步异常。结果可能不完整。", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant) }
        item { Text("${state.total} 封邮件 · 已加载 ${state.items.size} 封", style = MaterialTheme.typography.labelLarge, color = MaterialTheme.colorScheme.onSurfaceVariant) }
        if (state.loading) item { LinearProgressIndicator(Modifier.fillMaxWidth()) }
        if (state.error != null) item { ErrorMessage(state.error) { if (state.items.isEmpty()) controller.search(query, state.accountId) else controller.loadMore() } }
        if (!state.loading && state.error == null && state.items.isEmpty()) item { Text("暂无邮件\n尝试其他关键词或账户；首次同步完成后可查看结果。", Modifier.padding(vertical = 32.dp), color = MaterialTheme.colorScheme.onSurfaceVariant) }
        items(state.items, key = { it.id }) { message ->
            OutlinedCard(onClick = { controller.openMessage(message.id) }, modifier = Modifier.fillMaxWidth()) {
                Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Text(message.subject.ifBlank { "（无标题）" }, style = MaterialTheme.typography.titleMedium, maxLines = 2, overflow = TextOverflow.Ellipsis)
                    Text(message.fromAddress, style = MaterialTheme.typography.bodyMedium, maxLines = 1, overflow = TextOverflow.Ellipsis)
                    if (message.bodyPreview.isNotBlank()) Text(message.bodyPreview, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 3, overflow = TextOverflow.Ellipsis)
                    Text("${message.accountEmail} · ${formatDate(message.receivedAt)}", style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            }
        }
        if (state.nextOffset < state.total && state.error == null) item { OutlinedButton(onClick = controller::loadMore, enabled = !state.loading, modifier = Modifier.fillMaxWidth()) { Text("加载更多") } }
    }
}

@Composable private fun DetailScreen(state: SearchState, controller: SearchController, reply: ReplyController?, modifier: Modifier) {
    LazyColumn(modifier.fillMaxSize(), contentPadding = PaddingValues(24.dp), verticalArrangement = Arrangement.spacedBy(16.dp)) {
        if (state.detailLoading) item { LinearProgressIndicator(Modifier.fillMaxWidth()) }
        if (state.detailError != null) item { ErrorMessage(state.detailError) { state.selectedId?.let(controller::openMessage) } }
        state.detail?.let { message ->
            item { SelectionContainer { Text(message.subject.ifBlank { "（无标题）" }, style = MaterialTheme.typography.headlineSmall) } }
            item { SelectionContainer { Text("发件人：${message.fromAddress}\n收件人：${message.toAddresses.joinToString()}" + (if (message.ccAddresses.isEmpty()) "" else "\n抄送：${message.ccAddresses.joinToString()}") + "\n账户：${message.accountEmail}\n时间：${formatDate(message.receivedAt)}", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant) } }
            item {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Button(onClick = { reply?.start(message, state.canWrite) }, enabled = state.canWrite && reply != null) { Text("回复") }
                    if (!state.canWrite) Text("当前令牌仅可搜索阅读；回复请在设置中切换可写令牌。", style = MaterialTheme.typography.bodySmall)
                }
            }
            item { HorizontalDivider() }
            item { SelectionContainer { Text(message.bodyText.ifBlank { "（无纯文本正文）" }, style = MaterialTheme.typography.bodyLarge) } }
        }
    }
}

@Composable private fun ErrorMessage(message: String, retry: () -> Unit) {
    Column { Text(message, color = MaterialTheme.colorScheme.error); TextButton(onClick = retry) { Text("重试") } }
}
private fun formatDate(value: String): String = runCatching { OffsetDateTime.parse(value).atZoneSameInstant(java.time.ZoneId.systemDefault()).format(DateTimeFormatter.ofPattern("yyyy-MM-dd HH:mm")) }.getOrDefault(value)

@Composable private fun ReplyDialog(state: ReplyState, controller: ReplyController, canWrite: Boolean) {
    var body by remember(state.messageId) { mutableStateOf("") }
    var includeOriginal by remember(state.messageId) { mutableStateOf(true) }
    var confirmClear by remember { mutableStateOf(false) }
    if (confirmClear) {
        AlertDialog(onDismissRequest = { confirmClear = false }, title = { Text("已核对发送情况？") },
            text = { Text("清除本机记录不会撤回邮件或取消服务器操作。请先在邮箱或服务器核对；结果不明时重新回复可能重复发送。") },
            confirmButton = { TextButton(onClick = { controller.acknowledgeAndClear(); confirmClear = false }) { Text("已核对，清除记录") } },
            dismissButton = { TextButton(onClick = { confirmClear = false }) { Text("保留记录") } })
        return
    }
    AlertDialog(onDismissRequest = controller::hide, title = { Text(if (state.preview == null && state.pending == null && state.messageId != null) "回复邮件" else "回复确认") },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(12.dp)) {
                if (state.busy) LinearProgressIndicator(Modifier.fillMaxWidth())
                state.preview?.let { preview ->
                    SelectionContainer { Text("发件账户：${preview.accountEmail}\n收件人：${preview.to.joinToString()}\n主题：${preview.subject}") }
                    HorizontalDivider()
                    SelectionContainer { Text(preview.bodyText) }
                }
                if (state.preview == null && state.pending == null && state.messageId != null) {
                    OutlinedTextField(body, { body = it }, label = { Text("纯文本回复") }, minLines = 5, enabled = !state.busy, modifier = Modifier.fillMaxWidth())
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Checkbox(checked = includeOriginal, onCheckedChange = { includeOriginal = it }, enabled = !state.busy)
                        Text("包含原邮件")
                    }
                    Text("下一步先查看服务器生成的收件人和完整内容，再确认发送。", style = MaterialTheme.typography.bodySmall)
                }
                state.pending?.let { pending ->
                    SelectionContainer { Text("操作编号：${pending.operationId}\n服务器：${pending.serverUrl}", style = MaterialTheme.typography.bodySmall) }
                }
                val statusText = when (state.status) {
                    "prepared" -> if (state.preview != null) "已准备，请核对账户、收件人和正文后发送。" else "服务器已准备，但本机没有完整预览。请在服务器核对后清除本机记录。"
                    "sending" -> "正在发送或等待服务器确认，请查询原操作；不会自动重发。"
                    "sent" -> "服务器已确认发送。"
                    "failed" -> "服务器确认发送失败。请先核对邮箱，再清除记录后重新回复。"
                    "expired" -> "回复准备已过期；可清除记录后重新准备。"
                    "unknown" -> "发送结果尚未确认。请查询原操作，并在邮箱核对，避免重复发送。"
                    null -> null
                    else -> "服务器状态：${state.status}。请核对后再继续。"
                }
                statusText?.let { Text(it) }
                state.error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
                if (state.pending != null && !canWrite) Text("查询回复状态和发送都需要可写访问令牌，请前往设置切换。")
                if (state.pending != null) OutlinedButton(onClick = controller::checkStatus, enabled = !state.busy && canWrite) { Text("查询原操作状态") }
                if (state.pending != null || (state.messageId == null && state.error != null)) TextButton(onClick = { confirmClear = true }, enabled = !state.busy) { Text("已核对，清除本机记录…") }
            }
        },
        confirmButton = {
            if (state.status == "prepared" && state.preview != null) Button(onClick = { controller.send(canWrite) }, enabled = !state.busy && canWrite) { Text("确认发送") }
            else if (state.messageId != null && state.pending == null && state.status == null) Button(onClick = { controller.prepare(body, includeOriginal) }, enabled = !state.busy && body.isNotBlank() && canWrite) { Text("预览回复") }
        },
        dismissButton = { TextButton(onClick = controller::hide) { Text(if (state.status == "sent") "完成" else "返回") } })
}
