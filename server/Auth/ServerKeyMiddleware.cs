namespace Vorcall.Server.Auth;

public sealed class ServerKeyMiddleware(RequestDelegate next, ServerKeyValidator validator)
{
    public const string HeaderName = "X-Vorcall-Key";

    public async Task InvokeAsync(HttpContext context)
    {
        // A missing header arrives as an empty string and still goes through the fixed-time
        // comparison. The key itself is never logged, not even at Debug.
        if (!validator.IsValid(context.Request.Headers[HeaderName].ToString()))
        {
            // The scheme is how a client tells a stale build (wrong key) from an expired bearer.
            context.Response.Headers.WWWAuthenticate = HeaderName;
            context.Response.StatusCode = StatusCodes.Status401Unauthorized;
            return;
        }

        await next(context);
    }
}
