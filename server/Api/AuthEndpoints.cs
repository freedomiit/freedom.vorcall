using System.Globalization;
using Vorcall.Server.Auth;
using Vorcall.Server.Chat;
using Vorcall.Server.Protocol;

namespace Vorcall.Server.Api;

public static class AuthEndpoints
{
    // Shared with the rate limiter registration so both sides name the same policy.
    public const string RateLimitPolicy = "auth";

    // PROTOCOL.md § Moderation: the one 403 login and refresh ever answer, and what tells a client
    // to stop trying rather than sign in again.
    private const string BannedDetail = "banned";

    // An admin lock on the account rather than moderation: the same 403, its own detail.
    private const string DisabledDetail = "account disabled";

    public static void Map(WebApplication app)
    {
        app.MapPost("/api/auth/register", RegisterAsync).RequireRateLimiting(RateLimitPolicy);
        app.MapPost("/api/auth/login", LoginAsync).RequireRateLimiting(RateLimitPolicy);
        app.MapPost("/api/auth/refresh", RefreshAsync).RequireRateLimiting(RateLimitPolicy);
        app.MapPost("/api/auth/logout", LogoutAsync);
        app.MapPost("/api/auth/password", ChangePasswordAsync).RequireAuthorization();
    }

    private static async Task<IResult> RegisterAsync(
        HttpContext context,
        AccountService accounts,
        MemberDirectory members,
        ConnectionRegistry registry)
    {
        var body = await ProtobufBody.ReadAsync(context, RegisterRequest.Parser);
        if (body.Message is not { } request)
        {
            return body.Failure;
        }

        var outcome = await accounts.RegisterAsync(request.Username, request.Password, request.InviteCode, DateTime.UtcNow);
        if (outcome.Status != RegisterStatus.Registered)
        {
            return outcome.Status switch
            {
                RegisterStatus.InvalidUsername => ProtobufBody.Fail(
                    StatusCodes.Status400BadRequest,
                    "username must be 1..32 characters without control characters"),
                RegisterStatus.InvalidPassword => ProtobufBody.Fail(StatusCodes.Status400BadRequest, "password must be 8..128 characters"),
                RegisterStatus.InvalidInvite => ProtobufBody.Fail(StatusCodes.Status400BadRequest, "invite code must be 20 characters"),
                RegisterStatus.InviteUnusable => ProtobufBody.Fail(StatusCodes.Status403Forbidden, "invite code is invalid, used or expired"),
                _ => ProtobufBody.Fail(StatusCodes.Status409Conflict, "username is taken"),
            };
        }

        // The registry resolves permissions from its own mirror, so the new account has to be in it
        // before its first frame — and every online member learns about it here, offline, rather
        // than when it first connects. The row was just committed, so the read cannot miss.
        var tokens = outcome.Tokens!;
        if (await members.GetAsync(tokens.UserId, context.RequestAborted) is { } member)
        {
            registry.MemberRegistered(member);
        }

        return ProtobufBody.Proto(tokens, StatusCodes.Status201Created);
    }

    private static async Task<IResult> LoginAsync(HttpContext context, AccountService accounts)
    {
        var body = await ProtobufBody.ReadAsync(context, LoginRequest.Parser);
        if (body.Message is not { } request)
        {
            return body.Failure;
        }

        var outcome = await accounts.LoginAsync(request.Username, request.Password, DateTime.UtcNow);
        switch (outcome.Status)
        {
            case LoginStatus.Locked:
                context.Response.Headers.RetryAfter = outcome.RetryAfterSeconds.ToString(CultureInfo.InvariantCulture);
                return ProtobufBody.Fail(
                    StatusCodes.Status429TooManyRequests,
                    $"too many attempts, try again in {outcome.RetryAfterSeconds}s");

            // The same body whether or not the account exists.
            case LoginStatus.Invalid:
                return ProtobufBody.Fail(StatusCodes.Status401Unauthorized, "invalid username or password");

            // The credentials were right: 403, not 401, so the client stops retrying.
            case LoginStatus.Banned:
                return ProtobufBody.Fail(StatusCodes.Status403Forbidden, BannedDetail);

            // Only ever reached with the right password, so naming the reason tells the holder
            // of the account something they are entitled to know.
            case LoginStatus.Disabled:
                return ProtobufBody.Fail(StatusCodes.Status403Forbidden, DisabledDetail);

            default:
                return ProtobufBody.Proto(outcome.Tokens!);
        }
    }

    private static async Task<IResult> RefreshAsync(HttpContext context, AccountService accounts)
    {
        var body = await ProtobufBody.ReadAsync(context, RefreshRequest.Parser);
        if (body.Message is not { } request)
        {
            return body.Failure;
        }

        var outcome = await accounts.RefreshAsync(request.RefreshToken, DateTime.UtcNow);
        return outcome.Status switch
        {
            AccountRefreshStatus.Banned => ProtobufBody.Fail(StatusCodes.Status403Forbidden, BannedDetail),
            AccountRefreshStatus.Disabled => ProtobufBody.Fail(StatusCodes.Status403Forbidden, DisabledDetail),
            AccountRefreshStatus.Rotated => ProtobufBody.Proto(outcome.Tokens!),
            _ => ProtobufBody.Fail(StatusCodes.Status401Unauthorized, "refresh token is invalid"),
        };
    }

    private static async Task<IResult> LogoutAsync(HttpContext context, AccountService accounts)
    {
        var body = await ProtobufBody.ReadAsync(context, LogoutRequest.Parser);
        if (body.Message is { } request)
        {
            await accounts.LogoutAsync(request.RefreshToken, DateTime.UtcNow);
        }

        // Always 204: whether the token existed is not the caller's business, and a body this
        // server could not read is nothing to report on the way out.
        return Results.NoContent();
    }

    private static async Task<IResult> ChangePasswordAsync(HttpContext context, AccountService accounts)
    {
        var body = await ProtobufBody.ReadAsync(context, ChangePasswordRequest.Parser);
        if (body.Message is not { } request)
        {
            return body.Failure;
        }

        // A token this server signed always carries a numeric sub, so getting here is a bug on
        // this side and saying "wrong password" would send the user hunting for the wrong thing.
        if (!BearerIdentity.TryGetUserId(context.User, out var userId))
        {
            return ProtobufBody.Fail(StatusCodes.Status401Unauthorized, "invalid bearer");
        }

        var outcome = await accounts.ChangePasswordAsync(
            userId,
            request.CurrentPassword,
            request.NewPassword,
            request.RefreshToken,
            DateTime.UtcNow);

        return outcome switch
        {
            ChangePasswordOutcome.WrongPassword => ProtobufBody.Fail(StatusCodes.Status401Unauthorized, "current password is wrong"),
            ChangePasswordOutcome.InvalidPassword => ProtobufBody.Fail(StatusCodes.Status400BadRequest, "password must be 8..128 characters"),
            _ => Results.NoContent(),
        };
    }
}
