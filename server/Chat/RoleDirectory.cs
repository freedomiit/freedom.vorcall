using Microsoft.EntityFrameworkCore;
using Vorcall.Server.Attachments;
using Vorcall.Server.Data;
using Vorcall.Server.Permissions;

namespace Vorcall.Server.Chat;

// Permissions is the wire's uint64; the entity stores the same number as bigint. Position is the
// hierarchy, @everyone pinned at 0.
public sealed record RoleRecord(
    long Id,
    string Name,
    int? Color,
    string IconEmoji,
    long? IconImageId,
    int Position,
    ulong Permissions,
    bool Hoist,
    bool IsEveryone);

// Roles and the memberships that hold them. Hierarchy rules and grant checks live in the
// permission engine; this class only writes what the caller has already decided is allowed.
public sealed class RoleDirectory(IDbContextFactory<AppDbContext> contexts, ILogger<RoleDirectory> logger)
{
    // Positions above @everyone start here: the everyone row owns 0 alone.
    private const int LowestPosition = 1;

    // RolesByUser never lists @everyone: every account holds that one implicitly.
    public async Task<(List<RoleRecord> Roles, Dictionary<long, List<long>> RolesByUser)> LoadAllAsync(CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);

        var roles = await db.Roles
            .AsNoTracking()
            .OrderBy(r => r.Position)
            .ThenBy(r => r.Id)
            .ToListAsync(ct);

        var memberships = await db.MemberRoles
            .AsNoTracking()
            .OrderBy(m => m.UserId)
            .ThenBy(m => m.RoleId)
            .Select(m => new { m.UserId, m.RoleId })
            .ToListAsync(ct);

        var rolesByUser = memberships
            .GroupBy(m => m.UserId)
            .ToDictionary(byUser => byUser.Key, byUser => byUser.Select(m => m.RoleId).ToList());

        return (roles.Select(ToRecord).ToList(), rolesByUser);
    }

    // Everything at or above the insertion point moves up, so the new role lands exactly where
    // the caller's own highest position was.
    public async Task<RoleRecord> CreateAsync(
        string name,
        int? color,
        string iconEmoji,
        long? iconImageId,
        ulong permissions,
        bool hoist,
        int position,
        CancellationToken ct)
    {
        var at = Math.Max(position, LowestPosition);

        await using var db = await contexts.CreateDbContextAsync(ct);
        await using var transaction = await db.Database.BeginTransactionAsync(ct);

        await db.Roles
            .Where(r => !r.IsEveryone && r.Position >= at)
            .ExecuteUpdateAsync(setters => setters.SetProperty(r => r.Position, r => r.Position + 1), ct);

        var role = new Data.Role
        {
            Name = name,
            Color = color,
            IconEmoji = iconEmoji,
            IconImageId = iconImageId,
            Position = at,
            Permissions = (long)Perms.Clean(permissions),
            Hoist = hoist,
            IsEveryone = false,
        };

        db.Roles.Add(role);
        await db.SaveChangesAsync(ct);
        await transaction.CommitAsync(ct);
        logger.LogDebug("Role {RoleId} created at position {Position}", role.Id, role.Position);
        return ToRecord(role);
    }

    // Position is ignored: reordering is its own frame. On the everyone row only the permissions
    // are editable, which is the floor under the caller's own refusal. ReplacedIconId is the icon
    // this write left referenced by nothing, for the caller to delete now instead of at the next
    // sweep — the same answer the profile and server rows give.
    public async Task<(bool Found, long? ReplacedIconId)> UpdateAsync(RoleRecord role, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        await using var transaction = await db.Database.BeginTransactionAsync(ct);

        var row = await db.Roles.FirstOrDefaultAsync(r => r.Id == role.Id, ct);
        if (row is null)
        {
            return (false, null);
        }

        var previousIcon = row.IconImageId;

        row.Permissions = (long)Perms.Clean(role.Permissions);
        if (!row.IsEveryone)
        {
            row.Name = role.Name;
            row.Color = role.Color;
            row.IconEmoji = role.IconEmoji;
            row.IconImageId = role.IconImageId;
            row.Hoist = role.Hoist;
        }

        await db.SaveChangesAsync(ct);

        long? replaced = null;
        if (previousIcon is { } previous
            && previous != row.IconImageId
            && !await ImageStore.IsReferencedAsync(db, previous, ct))
        {
            replaced = previous;
        }

        await transaction.CommitAsync(ct);
        return (true, replaced);
    }

    // The memberships go with the row through the cascade; the channel overrides that named it
    // are the caller's to remove through ChannelDirectory.
    public async Task<bool> DeleteAsync(long id, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        await using var transaction = await db.Database.BeginTransactionAsync(ct);

        var everyone = await db.Roles
            .AsNoTracking()
            .Where(r => r.Id == id)
            .Select(r => (bool?)r.IsEveryone)
            .FirstOrDefaultAsync(ct);
        if (everyone is not false)
        {
            return false;
        }

        await db.Roles.Where(r => r.Id == id).ExecuteDeleteAsync(ct);

        // Positions stay dense, so the next insert lands where the caller meant it to.
        var rest = await db.Roles
            .Where(r => !r.IsEveryone)
            .OrderBy(r => r.Position)
            .ThenBy(r => r.Id)
            .ToListAsync(ct);
        var next = LowestPosition;
        foreach (var role in rest)
        {
            role.Position = next++;
        }

        await db.SaveChangesAsync(ct);
        await transaction.CommitAsync(ct);
        logger.LogDebug("Role {RoleId} deleted", id);
        return true;
    }

    // Bottom first, as the wire has it: position = index + 1, and @everyone keeps 0.
    public async Task<bool> ReorderAsync(IReadOnlyList<long> idsBottomFirst, CancellationToken ct)
    {
        if (idsBottomFirst.Count == 0)
        {
            return true;
        }

        var wanted = idsBottomFirst.ToList();
        if (wanted.Distinct().Count() != wanted.Count)
        {
            return false;
        }

        await using var db = await contexts.CreateDbContextAsync(ct);
        await using var transaction = await db.Database.BeginTransactionAsync(ct);

        var roles = await db.Roles.Where(r => wanted.Contains(r.Id)).ToListAsync(ct);
        if (roles.Count != wanted.Count || roles.Exists(r => r.IsEveryone))
        {
            return false;
        }

        var byId = roles.ToDictionary(r => r.Id);
        for (var index = 0; index < wanted.Count; index++)
        {
            byId[wanted[index]].Position = index + LowestPosition;
        }

        await db.SaveChangesAsync(ct);
        await transaction.CommitAsync(ct);
        return true;
    }

    // Replaces the member's roles. @everyone is implicit and never stored; an id that names no
    // role is dropped rather than written as a row the foreign key would refuse — the caller has
    // already answered UNKNOWN_ROLE for it.
    public async Task SetMemberRolesAsync(long userId, IReadOnlyCollection<long> roleIds, CancellationToken ct)
    {
        var wanted = roleIds.Distinct().ToList();

        await using var db = await contexts.CreateDbContextAsync(ct);
        await using var transaction = await db.Database.BeginTransactionAsync(ct);

        List<long> valid = wanted.Count == 0
            ? []
            : await db.Roles
                .AsNoTracking()
                .Where(r => !r.IsEveryone && wanted.Contains(r.Id))
                .Select(r => r.Id)
                .ToListAsync(ct);

        await db.MemberRoles.Where(m => m.UserId == userId).ExecuteDeleteAsync(ct);
        foreach (var roleId in valid)
        {
            db.MemberRoles.Add(new MemberRole { UserId = userId, RoleId = roleId });
        }

        await db.SaveChangesAsync(ct);
        await transaction.CommitAsync(ct);
    }

    // The roles one account holds, @everyone excluded like LoadAllAsync's map: an unban has to put
    // the member back into the mirror with the roles its rows still name.
    public async Task<List<long>> RolesOfAsync(long userId, CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        return await (from membership in db.MemberRoles.AsNoTracking()
                      join role in db.Roles.AsNoTracking() on membership.RoleId equals role.Id
                      where membership.UserId == userId && !role.IsEveryone
                      orderby role.Id
                      select role.Id).ToListAsync(ct);
    }

    public async Task<int> CountAsync(CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        return await db.Roles.CountAsync(ct);
    }

    // Null only on a database that was never seeded, which Program.cs rules out before anything
    // listens.
    public async Task<long?> EveryoneIdAsync(CancellationToken ct)
    {
        await using var db = await contexts.CreateDbContextAsync(ct);
        return await db.Roles
            .AsNoTracking()
            .Where(r => r.IsEveryone)
            .Select(r => (long?)r.Id)
            .FirstOrDefaultAsync(ct);
    }

    private static RoleRecord ToRecord(Data.Role role) => new(
        role.Id,
        role.Name,
        role.Color,
        role.IconEmoji,
        role.IconImageId,
        role.Position,
        Perms.Clean((ulong)role.Permissions),
        role.Hoist,
        role.IsEveryone);
}
