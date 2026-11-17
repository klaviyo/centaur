# Saves a Console skill and its owner-managed reader/editor grants atomically.
#
#   authoring = SkillAuthoring.new(skill, editor_ids: ["usr_…"]) # nil keeps editors
#   authoring.save # => true, or false with skill.errors populated
#
# Readers and editors are resolved by user OID among active users other than the owner;
# any unknown, inactive, duplicate-of-owner, or malformed id fails validation
# with "Editors include an unavailable user" instead of being dropped.
# Changing either access list bumps updated_at so catalog readers see the change.
class SkillAuthoring
  attr_reader :skill

  def initialize(skill, editor_ids: nil, reader_ids: nil)
    @skill = skill
    @editor_ids = editor_ids
    @reader_ids = reader_ids
  end

  def editors
    @editors ||= manage_editors? ? resolve_editors : []
  end

  # Raises ActiveRecord::StaleObjectError on a lock_version conflict.
  def save
    selected = editors
    selected_readers = @reader_ids.nil? ? nil : resolve_users(@reader_ids, :readers)
    return false unless skill.valid?

    if @editor_selection_invalid || @reader_selection_invalid
      skill.errors.add(@reader_selection_invalid ? :readers : :editors, "include an unavailable user")
      return false
    end

    if manage_editors?
      selected_ids = selected.map(&:id)
      skill.updated_at = Time.current if skill.editor_ids.sort != selected_ids.sort
    end

    if selected_readers && skill.reader_ids.sort != selected_readers.map(&:id).sort
      skill.updated_at = Time.current
    end

    Skill.transaction do
      skill.save!
      skill.editor_ids = selected.map(&:id) if manage_editors?
      skill.reader_ids = selected_readers.map(&:id) if selected_readers
    end
    true
  end

  private

  def manage_editors?
    !@editor_ids.nil?
  end

  def resolve_editors
    resolve_users(@editor_ids, :editors)
  end

  def resolve_users(oids, kind)
    submitted = Array(oids).map(&:to_s).compact_blank.uniq
    ids = submitted.filter_map { |oid| User.decode_oid(oid) }
    users_by_id = User.active.where(id: ids).index_by(&:id)
    users = ids.filter_map { |id| users_by_id[id] }.reject { |user| user.id == skill.user_id }

    instance_variable_set(kind == :readers ? :@reader_selection_invalid : :@editor_selection_invalid,
                          ids.length != submitted.length || users.length != ids.length)
    users
  end
end
