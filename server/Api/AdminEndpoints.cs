using System.Net.WebSockets;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using Vorcall.Server.Admin;
using Vorcall.Server.Chat;

namespace Vorcall.Server.Api;

// The admin CLI is a second process with no reach into this one's sockets, so kicking and
// banning end here. Two gates, both of which answer like a path that does not exist: the request
// has to come from the host itself, the docker network or a LAN, and it has to carry the admin
// key. nginx has no location for /api/admin either, so the public side cannot even knock.
public static class AdminEndpoints
{
    // Shared with the CLI, which is the only caller these endpoints have.
    public const string AdminKeyHeader = "X-Vorcall-Admin-Key";

    // A static class cannot be a type argument, so the log category is named rather than taken
    // from ILogger<T>.
    private const string LogCategory = "Vorcall.Server.Api.AdminEndpoints";

    public static void Map(WebApplication app)
    {
        var options = app.Services.GetRequiredService<AdminOptions>();
        var logger = app.Services.GetRequiredService<ILoggerFactory>().CreateLogger(LogCategory);

        // No key is a valid deployment: the CLI's database half still bans and revokes, and the
        // endpoints that would have to be secret are simply not there.
        if (options.Key is null)
        {
            logger.LogInformation("admin endpoints disabled: Vorcall:AdminKey is not set");
            return;
        }

        app.MapPost("/api/admin/kick", KickAsync);
        app.MapPost("/api/admin/refresh-account", RefreshAccountAsync);
    }

    // POST /api/admin/kick {"userId":42,"code":4001,"reason":"..."} -> 200 {"closed":true|false}.
    private static async Task<IResult> KickAsync(
        HttpContext context,
        AdminOptions options,
        ConnectionRegistry registry,
        ILoggerFactory loggers)
    {
        if (!IsAdmin(context, options))
        {
            return Results.NotFound();
        }

        if (await ReadAsync<KickRequest>(context) is not { } request || request.UserId <= 0)
        {
            return BadRequest("userId is required");
        }

        var status = (WebSocketCloseStatus)request.Code;
        if (status != VorcallCloseStatus.Kicked && status != VorcallCloseStatus.Disabled)
        {
            return BadRequest("code must be 4001 or 4003");
        }

        var reason = string.IsNullOrWhiteSpace(request.Reason)
            ? status == VorcallCloseStatus.Disabled ? "account disabled" : "kicked by admin"
            : request.Reason;

        var closed = await registry.DisconnectAsync(request.UserId, status, reason);
        loggers.CreateLogger(LogCategory).LogInformation(
            "Admin asked to close user {UserId} with {CloseCode}; live connection: {Closed}",
            request.UserId,
            request.Code,
            closed);
        return Results.Json(new { closed });
    }

    // POST /api/admin/refresh-account {"userId":42} -> 204, so a ban or an unban is visible to
    // the next request rather than at the end of the cache's own half minute.
    private static async Task<IResult> RefreshAccountAsync(
        HttpContext context,
        AdminOptions options,
        DisabledAccounts accounts,
        ILoggerFactory loggers)
    {
        if (!IsAdmin(context, options))
        {
            return Results.NotFound();
        }

        if (await ReadAsync<RefreshRequest>(context) is not { } request || request.UserId <= 0)
        {
            return BadRequest("userId is required");
        }

        accounts.Invalidate(request.UserId);
        loggers.CreateLogger(LogCategory).LogInformation("Admin refreshed the ban state of user {UserId}", request.UserId);
        return Results.NoContent();
    }

    private static bool IsAdmin(HttpContext context, AdminOptions options)
    {
        if (!PrivateSource.IsPrivate(context) || options.Key is not { } key)
        {
            return false;
        }

        var presented = context.Request.Headers[AdminKeyHeader].FirstOrDefault();
        return presented is not null
            && CryptographicOperations.FixedTimeEquals(
                Encoding.UTF8.GetBytes(presented),
                Encoding.UTF8.GetBytes(key));
    }

    // A body this server cannot read is the caller's mistake, not a 500: the CLI is the only
    // thing that speaks here and it deserves to be told which half it got wrong.
    private static async Task<T?> ReadAsync<T>(HttpContext context)
        where T : class
    {
        if (!context.Request.HasJsonContentType())
        {
            return null;
        }

        try
        {
            return await context.Request.ReadFromJsonAsync<T>(context.RequestAborted);
        }
        catch (JsonException)
        {
            return null;
        }
    }

    private static IResult BadRequest(string detail)
        => Results.Json(new { detail }, statusCode: StatusCodes.Status400BadRequest);

    private sealed record KickRequest(long UserId, int Code, string? Reason);

    private sealed record RefreshRequest(long UserId);
}
