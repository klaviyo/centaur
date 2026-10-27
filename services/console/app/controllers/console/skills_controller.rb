class Console::SkillsController < ApplicationController
  layout "console"

  def index
    load_index("directory")
  end

  def mine
    load_index("mine")
    render :index
  end

  def show
    @skill = readable_skill
  end

  def new
    @skill = current_user.skills.new(content: "# Instructions")
    prepare_access_pickers
  end

  def create
    attributes = skill_params
    editor_oids = attributes.delete(:editor_oids)
    reader_oids = attributes.delete(:reader_oids)
    @skill = current_user.skills.new(attributes)
    prepare_access_pickers(editor_oids, reader_oids)
    if save_with_access
      redirect_to console_skill_path(@skill.oid), notice: "Skill created."
    else
      render :new, status: :unprocessable_entity
    end
  end

  def edit
    @skill = editable_skill
    prepare_access_pickers
  end

  def update
    @skill = editable_skill
    attributes = skill_params
    editor_oids = attributes.delete(:editor_oids)
    reader_oids = attributes.delete(:reader_oids)
    attributes.delete(:visibility) unless owner?
    @skill.assign_attributes(attributes)
    prepare_access_pickers(owner? ? editor_oids : nil, owner? ? reader_oids : nil)
    if save_with_access(manage_access: owner?)
      redirect_to console_skill_path(@skill.oid), notice: "Skill saved."
    else
      render :edit, status: :unprocessable_entity
    end
  rescue ActiveRecord::StaleObjectError
    redirect_to edit_console_skill_path(params[:id]), alert: "This skill changed since you opened it. Review the latest content before saving."
  end

  def destroy
    owned_skill.archive!
    redirect_to mine_console_skills_path, notice: "Skill archived."
  end

  def share
    owned_skill.share!
    redirect_to console_skill_path(params[:id]), notice: "Skill is now shared."
  rescue ActiveRecord::RecordInvalid => e
    redirect_to console_skill_path(params[:id]), alert: e.record.errors.full_messages.to_sentence
  end

  def unshare
    skill = moderation_skill
    skill.unshare!
    redirect_to console_skill_path(skill.oid), notice: "Skill is now private."
  end

  private

  def load_index(tab)
    @tab = tab
    @query = params[:q].to_s.strip
    @skills = if @tab == "mine"
      Skill.catalog_visible_to(current_user).where(visibility: "private")
        .or(Skill.editable_by(current_user)).includes(:user)
    else
      Skill.active.shared.includes(:user)
    end
    @skills = @skills.search(@query) if @query.present?
    @skills = @skills.order(updated_at: :desc, id: :asc)
  end

  def readable_skill
    Skill.catalog_visible_to(current_user).find_by_oid!(params[:id])
  end

  def owned_skill
    current_user.skills.active.find_by_oid!(params[:id])
  end

  def editable_skill
    Skill.editable_by(current_user).find_by_oid!(params[:id])
  end

  def moderation_skill
    return owned_skill unless acting_admin?

    Skill.active.shared.find_by_oid!(params[:id])
  end

  def skill_params
    params.require(:skill).permit(:name, :description, :content, :visibility, :lock_version, editor_oids: [], reader_oids: [])
  end

  def owner?
    @skill.user_id == current_user.id
  end

  def prepare_access_pickers(submitted_oids = nil, submitted_reader_oids = nil)
    @editor_candidates = User.active.where.not(id: @skill.user_id || current_user.id).order(:email)
    @selected_editors = if submitted_oids.nil?
      @skill.persisted? ? @skill.editors.active.order(:email).to_a : []
    else
      resolve_access_users(submitted_oids, :editors)
    end
    @selected_readers = if submitted_reader_oids.nil?
      @skill.persisted? ? @skill.readers.active.order(:email).to_a : []
    else
      resolve_access_users(submitted_reader_oids, :readers)
    end
  end

  def resolve_access_users(oids, permission)
    submitted = Array(oids).compact_blank.uniq
    ids = submitted.filter_map { |oid| User.decode_oid(oid) }
    users_by_id = User.active.where(id: ids).index_by(&:id)
    users = ids.filter_map { |id| users_by_id[id] }.reject { |user| user.id == @skill.user_id }

    if ids.length != submitted.length || users.length != ids.length
      @skill.errors.add(permission, "include an unavailable user")
      @access_selection_invalid = permission
    end
    users
  end

  def save_with_access(manage_access: true)
    return false unless @skill.valid?
    if @access_selection_invalid
      @skill.errors.add(@access_selection_invalid, "include an unavailable user")
      return false
    end

    if manage_access
      selected_ids = @selected_editors.map(&:id)
      if @skill.editor_ids.sort != selected_ids.sort || @skill.reader_ids.sort != @selected_readers.map(&:id).sort
        @skill.updated_at = Time.current
      end
    end

    Skill.transaction do
      @skill.save!
      if manage_access
        @skill.editor_ids = @selected_editors.map(&:id)
        @skill.reader_ids = @selected_readers.map(&:id)
      end
    end
    true
  end
end
