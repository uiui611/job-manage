namespace JobWorker.UI;

internal sealed class MainForm : Form
{
    private readonly string settingsPath = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "JobManage", "ui-settings.json");
    private readonly WorkerProcess worker = new();
    private readonly Dictionary<string, TextBox> fields = new();
    private readonly TableLayoutPanel settings = new() { Dock = DockStyle.Top, AutoSize = true, ColumnCount = 3, Padding = new Padding(16) };
    private readonly CheckBox allowHttp = new() { Text = "開発用 HTTP を許可", AutoSize = true };
    private readonly Button start = new() { Text = "起動", AutoSize = true };
    private readonly Button stop = new() { Text = "停止", AutoSize = true, Enabled = false };
    private readonly Button save = new() { Text = "設定を保存", AutoSize = true };
    private readonly Label status = new() { Text = "停止中", AutoSize = true, Padding = new Padding(8) };
    private readonly TextBox logs = new() { Multiline = true, ReadOnly = true, ScrollBars = ScrollBars.Both, WordWrap = false, Dock = DockStyle.Fill, MaxLength = 0 };
    private readonly System.Windows.Forms.Timer timer = new() { Interval = 200 };
    private bool busy;
    private bool canClose;

    public MainForm()
    {
        Text = "Job Manage — Windows Worker";
        AutoScaleMode = AutoScaleMode.Dpi;
        Font = new Font("Yu Gothic UI", 10);
        ClientSize = new Size(920, 820);
        MinimumSize = new Size(720, 620);
        StartPosition = FormStartPosition.CenterScreen;
        var layout = new TableLayoutPanel { Dock = DockStyle.Fill, RowCount = 4, ColumnCount = 1 };
        layout.RowStyles.Add(new RowStyle(SizeType.Absolute, 54));
        layout.RowStyles.Add(new RowStyle(SizeType.Percent, 62));
        layout.RowStyles.Add(new RowStyle(SizeType.Absolute, 52));
        layout.RowStyles.Add(new RowStyle(SizeType.Percent, 38));
        layout.Controls.Add(new Label { Text = "Windows Worker  /  接続設定と実行ログ", Dock = DockStyle.Fill, Padding = new Padding(16), AutoSize = true }, 0, 0);
        settings.ColumnStyles.Add(new ColumnStyle(SizeType.Absolute, 155));
        settings.ColumnStyles.Add(new ColumnStyle(SizeType.Percent, 100));
        settings.ColumnStyles.Add(new ColumnStyle(SizeType.Absolute, 76));
        var panel = new Panel { Dock = DockStyle.Fill, AutoScroll = true };
        panel.Controls.Add(settings);
        layout.Controls.Add(panel, 0, 1);
        AddField("ServerUrl", "接続先 URL");
        AddField("Token", "ワーカートークン");
        fields["Token"].UseSystemPasswordChar = true;
        fields["Token"].MaxLength = 4096;
        AddField("WorkerId", "ワーカー ID");
        AddField("Name", "表示名");
        AddField("Tags", "タグ（JSON 配列）");
        AddField("WorkDir", "作業フォルダー", folder: true);
        AddField("NodePath", "Node 実行ファイル", file: true);
        AddField("PythonPath", "Python 実行ファイル", file: true);
        AddField("CaFile", "追加 CA（任意）", file: true);
        AddField("EnvironmentAllowlist", "追加環境変数名");
        settings.Controls.Add(allowHttp, 1, settings.RowCount++);
        var hint = new Label { Text = "トークンは保存されません。接続状況・エラーは下のログで確認できます。", AutoSize = true, Margin = new Padding(0, 10, 0, 4) };
        settings.Controls.Add(hint, 0, settings.RowCount++);
        settings.SetColumnSpan(hint, 3);
        var buttons = new FlowLayoutPanel { Dock = DockStyle.Fill, Padding = new Padding(16, 4, 0, 0) };
        buttons.Controls.AddRange([start, stop, save, status]);
        layout.Controls.Add(buttons, 0, 2);
        var logPanel = new Panel { Dock = DockStyle.Fill, Padding = new Padding(16, 0, 16, 16) };
        logPanel.Controls.Add(logs);
        layout.Controls.Add(logPanel, 0, 3);
        Controls.Add(layout);
        start.Click += (_, _) => StartWorker();
        stop.Click += async (_, _) => await StopWorker();
        save.Click += (_, _) => SaveSettings();
        timer.Tick += async (_, _) => await TickWorker();
        FormClosing += OnClosing;
        FormClosed += (_, _) => timer.Dispose();
        try { ShowSettings(Settings.Load(settingsPath)); }
        catch (Exception ex) { ShowSettings(new Settings()); Shown += (_, _) => ShowError("設定を読み込めませんでした。", ex); }
        timer.Start();
    }

    private void AddField(string key, string label, bool folder = false, bool file = false)
    {
        int row = settings.RowCount++;
        settings.RowStyles.Add(new RowStyle(SizeType.AutoSize));
        settings.Controls.Add(new Label { Text = label, AutoSize = true, Anchor = AnchorStyles.Left, Margin = new Padding(0, 8, 8, 8) }, 0, row);
        var input = new TextBox { Dock = DockStyle.Fill, Margin = new Padding(0, 5, 6, 5), AccessibleName = label };
        fields.Add(key, input);
        settings.Controls.Add(input, 1, row);
        if (!folder && !file) { settings.SetColumnSpan(input, 2); return; }
        var browse = new Button { Text = "参照…", AutoSize = true, AccessibleName = label + "を選択" };
        browse.Click += (_, _) =>
        {
            if (folder)
            {
                using var dialog = new FolderBrowserDialog();
                if (dialog.ShowDialog(this) == DialogResult.OK) input.Text = dialog.SelectedPath;
            }
            else
            {
                using var dialog = new OpenFileDialog { Filter = key == "CaFile" ? "PEM 証明書|*.pem;*.crt|すべてのファイル|*.*" : "実行ファイル|*.exe|すべてのファイル|*.*" };
                if (dialog.ShowDialog(this) == DialogResult.OK) input.Text = dialog.FileName;
            }
        };
        settings.Controls.Add(browse, 2, row);
    }

    private Settings ReadSettings() => new()
    {
        ServerUrl = fields["ServerUrl"].Text.Trim(), WorkerId = fields["WorkerId"].Text.Trim(),
        Name = fields["Name"].Text.Trim(), Tags = fields["Tags"].Text.Trim(),
        WorkDir = fields["WorkDir"].Text.Trim(), NodePath = fields["NodePath"].Text.Trim(),
        PythonPath = fields["PythonPath"].Text.Trim(), CaFile = fields["CaFile"].Text.Trim(),
        EnvironmentAllowlist = fields["EnvironmentAllowlist"].Text.Trim(), AllowHttp = allowHttp.Checked
    };

    private void ShowSettings(Settings value)
    {
        foreach (var property in typeof(Settings).GetProperties())
            if (fields.TryGetValue(property.Name, out var field)) field.Text = property.GetValue(value)?.ToString() ?? "";
        allowHttp.Checked = value.AllowHttp;
    }

    private void SaveSettings()
    {
        try
        {
            var value = ReadSettings();
            // Saving non-secret settings does not require entering a token.
            value.Validate("validation-placeholder");
            value.Save(settingsPath);
            status.Text = "設定を保存しました";
        }
        catch (Exception ex) { ShowError("設定を保存できませんでした。", ex); }
    }

    private void StartWorker()
    {
        try
        {
            var executable = Path.Combine(AppContext.BaseDirectory, "job-worker.exe");
            if (!File.Exists(executable)) throw new FileNotFoundException("同じフォルダーに job-worker.exe を配置してください。");
            logs.Clear();
            worker.Start(ReadSettings(), fields["Token"].Text, executable);
            SetRunning(true);
            status.Text = "ワーカー起動中（接続状況はログ参照）";
        }
        catch (Exception ex) { ShowError("起動できませんでした。", ex); }
    }

    private void SetRunning(bool running)
    {
        settings.Enabled = !running;
        start.Enabled = save.Enabled = !running;
        stop.Enabled = running;
    }

    private async Task StopWorker()
    {
        if (busy) return;
        busy = true;
        stop.Enabled = false;
        status.Text = "停止中…";
        try
        {
            await worker.StopAsync();
            SetRunning(false);
            status.Text = "停止しました";
        }
        catch (Exception ex) { stop.Enabled = worker.IsRunning; ShowError("停止処理に失敗しました。", ex); }
        finally { busy = false; AppendLogs(); }
    }

    private async Task TickWorker()
    {
        if (busy) return;
        if (worker.HasProcess && !worker.IsRunning)
        {
            busy = true;
            try
            {
                var code = worker.ExitCode;
                await worker.ReapAsync();
                SetRunning(false);
                status.Text = code == 0 ? "停止しました" : $"エラー終了（コード {code}）";
            }
            finally { busy = false; }
        }
        AppendLogs();
    }

    private void AppendLogs()
    {
        var text = worker.DrainLogs();
        if (text.Length == 0) return;
        logs.AppendText(text);
        const int limit = 128 * 1024;
        if (logs.TextLength > limit) logs.Text = "[古いログを省略しました]\r\n" + logs.Text[^limit..];
        logs.SelectionStart = logs.TextLength;
        logs.ScrollToCaret();
    }

    private async void OnClosing(object? sender, FormClosingEventArgs e)
    {
        if (canClose || !worker.HasProcess) return;
        e.Cancel = true;
        if (busy) return;
        if (MessageBox.Show(this, "ワーカーと実行中のジョブを停止して終了しますか？", "終了確認", MessageBoxButtons.YesNo, MessageBoxIcon.Question) != DialogResult.Yes) return;
        await StopWorker();
        if (!worker.HasProcess) { canClose = true; Close(); }
    }

    private void ShowError(string message, Exception ex) => MessageBox.Show(this,
        message + "\n" + ex.Message, "Job Manage", MessageBoxButtons.OK, MessageBoxIcon.Error);
}
