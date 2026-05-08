using System.Windows;

namespace FastSearch;

public partial class App : Application
{
    private SearchBackend? _backend;
    private MainWindow? _window;

    protected override async void OnStartup(StartupEventArgs e)
    {
        base.OnStartup(e);
        _backend = new SearchBackend();
        _window = new MainWindow(_backend);
        _window.EnsureNativeWindow();
        _window.ShowSearch();
        await _backend.StartAsync();
    }

    protected override async void OnExit(ExitEventArgs e)
    {
        if (_backend is not null) await _backend.DisposeAsync();
        base.OnExit(e);
    }
}
