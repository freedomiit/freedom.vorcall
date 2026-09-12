namespace Vorcall.Server.Admin;

// Registered for both roles: the web path needs the ban check on every token it validates, and
// the CLI needs the key and URL to reach the running server.
public static class AdminSetup
{
    public static void Configure(WebApplicationBuilder builder)
    {
        builder.Services.AddSingleton(AdminOptions.FromConfiguration(builder.Configuration));
        builder.Services.AddSingleton<DisabledAccounts>();
    }
}
