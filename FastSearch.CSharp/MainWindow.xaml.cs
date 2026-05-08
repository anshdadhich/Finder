using System.Collections.ObjectModel;
using System.Runtime.InteropServices;
using System.Windows;
using System.Windows.Input;
using System.Windows.Interop;
using System.Windows.Threading;
using FastSearch.Index;

namespace FastSearch;

public partial class MainWindow : Window
{
    private const int HotkeyIdWinSpace = 0x4653;
    private const int HotkeyIdCtrlSpace = 0x4654;
    private const uint ModWin = 0x0008;
    private const uint ModCtrl = 0x0002;
    private const uint ModNoRepeat = 0x4000;
    private const uint VkSpace = 0x20;
    private const int WmHotkey = 0x0312;

    private readonly SearchBackend _backend;
    private readonly ObservableCollection<ResultItem> _items = [];
    private readonly DispatcherTimer _searchTimer;
    private HwndSource? _source;

    public MainWindow(SearchBackend backend)
    {
        InitializeComponent();
        _backend = backend;
        ResultsList.ItemsSource = _items;
        StatusText.Text = _backend.StatusText;
        _backend.StatusChanged += () => Dispatcher.Invoke(UpdateStatus);
        _searchTimer = new DispatcherTimer { Interval = TimeSpan.FromMilliseconds(12) };
        _searchTimer.Tick += (_, _) =>
        {
            _searchTimer.Stop();
            RunSearch();
        };
    }

    protected override void OnSourceInitialized(EventArgs e)
    {
        base.OnSourceInitialized(e);
        var handle = new WindowInteropHelper(this).Handle;
        _source = HwndSource.FromHwnd(handle);
        _source?.AddHook(WndProc);
        RegisterHotkeys(handle);
    }

    protected override void OnClosed(EventArgs e)
    {
        var handle = new WindowInteropHelper(this).Handle;
        UnregisterHotKey(handle, HotkeyIdWinSpace);
        UnregisterHotKey(handle, HotkeyIdCtrlSpace);
        _source?.RemoveHook(WndProc);
        base.OnClosed(e);
    }

    public void EnsureNativeWindow()
    {
        _ = new WindowInteropHelper(this).EnsureHandle();
    }

    public void ShowSearch()
    {
        SearchBox.Text = "";
        _items.Clear();
        UpdateStatus();
        Show();
        Activate();
        SearchBox.Focus();
    }

    private IntPtr WndProc(IntPtr hwnd, int msg, IntPtr wParam, IntPtr lParam, ref bool handled)
    {
        if (msg == WmHotkey && (wParam.ToInt32() == HotkeyIdWinSpace || wParam.ToInt32() == HotkeyIdCtrlSpace))
        {
            Toggle();
            handled = true;
        }

        return IntPtr.Zero;
    }

    private void Toggle()
    {
        if (IsVisible)
        {
            Hide();
            return;
        }

        SearchBox.Text = "";
        _items.Clear();
        UpdateStatus();
        WindowStartupLocation = WindowStartupLocation.CenterScreen;
        Show();
        Activate();
        SearchBox.Focus();
    }

    private void RegisterHotkeys(IntPtr handle)
    {
        var winSpaceOk = RegisterHotKey(handle, HotkeyIdWinSpace, ModWin | ModNoRepeat, VkSpace);
        if (!winSpaceOk)
        {
            StatusText.Text = "Win+Space is reserved by Windows. Use Ctrl+Space.";
        }

        RegisterHotKey(handle, HotkeyIdCtrlSpace, ModCtrl | ModNoRepeat, VkSpace);
    }

    private void SearchBox_TextChanged(object sender, System.Windows.Controls.TextChangedEventArgs e)
    {
        _searchTimer.Stop();
        _searchTimer.Start();
    }

    private void SearchBox_PreviewKeyDown(object sender, KeyEventArgs e)
    {
        if (HandleKeys(e)) return;
    }

    private void ResultsList_PreviewKeyDown(object sender, KeyEventArgs e)
    {
        if (HandleKeys(e)) return;
    }

    private bool HandleKeys(KeyEventArgs e)
    {
        if (e.Key == Key.Escape)
        {
            Hide();
            e.Handled = true;
            return true;
        }

        if (e.Key == Key.Enter)
        {
            OpenSelected(Keyboard.Modifiers.HasFlag(ModifierKeys.Control));
            e.Handled = true;
            return true;
        }

        if (e.Key == Key.Down && ResultsList.SelectedIndex < _items.Count - 1)
        {
            ResultsList.SelectedIndex++;
            ResultsList.ScrollIntoView(ResultsList.SelectedItem);
            e.Handled = true;
            return true;
        }

        if (e.Key == Key.Up && ResultsList.SelectedIndex > 0)
        {
            ResultsList.SelectedIndex--;
            ResultsList.ScrollIntoView(ResultsList.SelectedItem);
            e.Handled = true;
            return true;
        }

        return false;
    }

    private void ResultsList_MouseDoubleClick(object sender, MouseButtonEventArgs e)
    {
        OpenSelected(parent: false);
    }

    private void RunSearch()
    {
        var results = _backend.Search(SearchBox.Text, 50);
        _items.Clear();
        foreach (var result in results)
        {
            _items.Add(new ResultItem(result));
        }

        ResultsList.SelectedIndex = _items.Count == 0 ? -1 : 0;
        UpdateStatus();
    }

    private void OpenSelected(bool parent)
    {
        if (ResultsList.SelectedItem is not ResultItem item) return;
        if (parent) _backend.OpenParent(item.Path);
        else _backend.Open(item.Path);
        Hide();
    }

    private void UpdateStatus()
    {
        StatusText.Text = _items.Count == 0 ? _backend.StatusText : "";
        StatusText.Visibility = _items.Count == 0 ? Visibility.Visible : Visibility.Collapsed;
    }

    [DllImport("user32.dll", SetLastError = true)]
    private static extern bool RegisterHotKey(IntPtr hWnd, int id, uint fsModifiers, uint vk);

    [DllImport("user32.dll", SetLastError = true)]
    private static extern bool UnregisterHotKey(IntPtr hWnd, int id);
}

public sealed class ResultItem
{
    public ResultItem(SearchResult result)
    {
        Name = result.Name;
        Path = result.FullPath;
        KindText = result.IsDir ? "DIR" : "FILE";
    }

    public string Name { get; }
    public string Path { get; }
    public string KindText { get; }
}
