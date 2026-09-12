using System.Globalization;
using Vorcall.Server.Auth;
using Vorcall.Server.Protocol;

namespace Vorcall.Server.Api;

public static class AuthEndpoints
{
    // Shared with the rate limiter registration so both sides name the same policy.
    public const string RateLimitPolicy = "auth";

    public static void Map(WebApplication app)
    {
        app.MapPost("/api/auth/register", RegisterAsync).RequireRateLimiting(RateLimitPolicy);
        app.MapPost("/api/auth/login", LoginAsync).RequireRateLimiting(RateLimitPolicy);
        app.MapPost("/api/auth/refresh", RefreshAsync).RequireRateLimiting(RateLimitPolicy);
        app.MapPost("/api/auth/logout", LogoutAsync);
        app.MapPost("/api/auth/password", ChangePasswordAsync).RequireAuthorization();
    }

    private static async Task<IResult> RegisterAsync(HttpContext context, AccountService accounts)
    {
        var body = await ProtobufBody.ReadAsync(context, RegisterRequest.Parser);
        if (body.Message is not { } request)
        {
            return body.Failure;
        }

        var outcome = await accounts.RegisterAsync(request.Username, request.Password, request.InviteCode, DateTime.UtcNow);
        return outcome.Status switch
        {
            RegisterStatus.InvalidUsername => ProtobufBody.Fail(
                StatusCodes.Status400BadRequest,
                "username must be 1..32 characters without control characters"),
            RegisterStatus.InvalidPassword => ProtobufBody.Fail(StatusCodes.Status400BadRequest, "password must be 8..128 characters"),
            RegisterStatus.InvalidInvite => ProtobufBody.Fail(StatusCodes.Status400BadRequest, "invite code must be 20 characters"),
            RegisterStatus.InviteUnusable => ProtobufBody.Fail(StatusCodes.Status403Forbidden, "invite code is invalid, used or expired"),
            RegisterStatus.UsernameTaken => ProtobufBody.Fail(StatusCodes.Status409Conflict, "username is taken"),
            _ => ProtobufBody.Proto(outcome.Tokens!, StatusCodes.Status201Created),
        };
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

            // Only ever reached with the right password, so naming the reason tells the holder
            // of the account something they are entitled to know.
            case LoginStatus.Disabled:
                return ProtobufBody.Fail(StatusCodes.Status403Forbidden, "account disabled");

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

        var response = await accounts.RefreshAsync(request.RefreshToken, DateTime.UtcNow);
        return response is null
            ? ProtobufBody.Fail(StatusCodes.Status401Unauthorized, "refresh token is invalid")
            : ProtobufBody.Proto(response);
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
